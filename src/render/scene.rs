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

use super::gl::{bytemuck_cast, create_texture, create_texture_ex, Filter, Program};
use super::keytrack::KeyTrack;
use super::scenescript::{
    AnimLayerField, AnimLayerSeed, CursorEventInput, CursorEventKind, EffectSeed, LayerFrame,
    LayerSeed, MediaEventInput, PropAnimSeed, SceneScripts, ScriptField, ScriptOutput,
    ScriptTarget, SoundSeed, TexAnimSeed, VideoTexSeed,
};
use super::text;
use super::we_effect::{
    parse_usertexture_binding, Canvas, EffectContext, EffectInstance, SystemTexture, TextureCache,
    UserTextureSlot,
};
use super::we_model::{parse_mdl, Mesh, MeshVertex, ParsedMdl, PuppetAnimLayer};
use super::we_particle::{
    sample_layer_image_mask, ChildSpawnMode, Emitter, LayerImageMask, ParticleKind,
};
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
uniform sampler2D u_bloom;
uniform int u_has_bloom;
uniform vec4 u_bcs_hue; // x=brightness(-1..1), y=contrast(1), z=saturation(1), w=hue(rad)
out vec4 fragColor;
void main() {
    vec4 c = texture(u_tex, v_uv);
    if (u_has_bloom != 0) {
        c.rgb += texture(u_bloom, v_uv).rgb;
    }
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

const BLOOM_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos.x, -a_pos.y, 0.0, 1.0);
}
"#;

const BLOOM_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
uniform int u_mode; // 0=sdr_prefilter, 1=sdr_blur13, 2=hdr_prefilter, 3=box4
uniform vec2 u_texel_step;
uniform float u_threshold;
uniform float u_strength;
uniform vec3 u_tint;
uniform vec4 u_blend_params;
out vec4 fragColor;

void main() {
    if (u_mode == 1) {
        const float w[13] = float[13](
            0.006299, 0.017298, 0.039533, 0.075189,
            0.119007, 0.156756, 0.171834, 0.156756,
            0.119007, 0.075189, 0.039533, 0.017298, 0.006299
        );
        vec3 acc = vec3(0.0);
        for (int i = 0; i < 13; i++) {
            acc += texture(u_tex, v_uv + u_texel_step * float(i - 6)).rgb * w[i];
        }
        fragColor = vec4(acc, 1.0);
        return;
    }

    vec3 albedo = texture(u_tex, v_uv + vec2(-u_texel_step.x, -u_texel_step.y)).rgb
                + texture(u_tex, v_uv + vec2( u_texel_step.x, -u_texel_step.y)).rgb
                + texture(u_tex, v_uv + vec2(-u_texel_step.x,  u_texel_step.y)).rgb
                + texture(u_tex, v_uv + vec2( u_texel_step.x,  u_texel_step.y)).rgb;
    albedo *= 0.25;

    if (u_mode == 0) {
        float scale = max(albedo.r, max(albedo.g, albedo.b));
        albedo *= clamp(scale - u_threshold, 0.0, 1.0);
        float grayscale = dot(vec3(0.2989, 0.5870, 0.1140), albedo);
        albedo = -grayscale + albedo * 2.0;
        albedo = max(vec3(0.0), albedo * u_strength * u_tint);
    } else if (u_mode == 2) {
        albedo = max(vec3(0.0), albedo);
        float brightness = max(albedo.r, max(albedo.g, albedo.b));
        float soft = brightness - u_blend_params.y;
        soft = clamp(soft, 0.0, u_blend_params.z);
        soft = soft * soft * u_blend_params.w;
        float contribution = max(soft, brightness - u_blend_params.x);
        contribution /= max(brightness, 0.00001);
        albedo *= contribution * u_strength * u_tint;
    }

    fragColor = vec4(albedo, 1.0);
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
pub(super) struct Affine {
    pub(super) a: f32,
    pub(super) b: f32,
    pub(super) c: f32,
    pub(super) d: f32,
    pub(super) tx: f32,
    pub(super) ty: f32,
}

impl Affine {
    pub(super) fn identity() -> Self {
        Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 }
    }
    pub(super) fn translate(x: f32, y: f32) -> Self {
        Self { tx: x, ty: y, ..Self::identity() }
    }
    pub(super) fn rotate(r: f32) -> Self {
        let (s, c) = r.sin_cos();
        Self { a: c, b: s, c: -s, d: c, tx: 0.0, ty: 0.0 }
    }
    pub(super) fn scale(x: f32, y: f32) -> Self {
        Self { a: x, b: 0.0, c: 0.0, d: y, tx: 0.0, ty: 0.0 }
    }
    /// `self * rhs`: apply `rhs` first, then `self`.
    pub(super) fn mul(self, rhs: Self) -> Self {
        Self {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            tx: self.a * rhs.tx + self.c * rhs.ty + self.tx,
            ty: self.b * rhs.tx + self.d * rhs.ty + self.ty,
        }
    }
    pub(super) fn apply(self, p: [f32; 2]) -> [f32; 2] {
        [
            self.a * p[0] + self.c * p[1] + self.tx,
            self.b * p[0] + self.d * p[1] + self.ty,
        ]
    }
    pub(super) fn apply_vec(self, v: [f32; 2]) -> [f32; 2] {
        [self.a * v[0] + self.c * v[1], self.b * v[0] + self.d * v[1]]
    }
    pub(super) fn invert_point(self, p: [f32; 2]) -> Option<[f32; 2]> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-6 {
            return None;
        }
        let dx = p[0] - self.tx;
        let dy = p[1] - self.ty;
        Some([
            (self.d * dx - self.c * dy) / det,
            (-self.b * dx + self.a * dy) / det,
        ])
    }
}

/// Build the local (parent-relative) transform from Wallpaper Engine's
/// translation / rotation / scale triple.
fn compose_local(origin: [f32; 3], scale: [f32; 3], angles: [f32; 3]) -> Affine {
    Affine::translate(origin[0], origin[1])
        .mul(Affine::rotate(angles[2]))
        .mul(Affine::scale(scale[0], scale[1]))
}

#[derive(Debug, Clone)]
struct BloomConfig {
    enabled: bool,
    hdr: bool,
    strength: f32,
    threshold: f32,
    tint: [f32; 3],
    hdr_strength: f32,
    hdr_threshold: f32,
    hdr_feather: f32,
    hdr_scatter: f32,
    hdr_iterations: usize,
}

impl Default for BloomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hdr: false,
            strength: 2.0,
            threshold: 0.65,
            tint: [1.0, 1.0, 1.0],
            hdr_strength: 2.0,
            hdr_threshold: 1.0,
            hdr_feather: 0.1,
            hdr_scatter: 1.619,
            hdr_iterations: 8,
        }
    }
}

fn parse_bloom_config(
    general: &serde_json::Value,
    props: &HashMap<String, serde_json::Value>,
) -> BloomConfig {
    let def = BloomConfig::default();
    let resolve_bool = |key: &str, d: bool| {
        general
            .get(key)
            .map(|v| resolve_prop(v, props))
            .and_then(|v| value_bool(&v))
            .unwrap_or(d)
    };
    let resolve_f32 = |key: &str, d: f32| {
        general
            .get(key)
            .map(|v| resolve_prop(v, props))
            .and_then(|v| value_f32(&v))
            .unwrap_or(d)
    };
    let tint = general
        .get("bloomtint")
        .map(|v| parse_color(&resolve_prop(v, props), def.tint))
        .unwrap_or(def.tint);
    let hdr_iterations = general
        .get("bloomhdriterations")
        .map(|v| resolve_prop(v, props))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| value_f32(&v).map(|f| f.round() as i64))
        })
        .unwrap_or(def.hdr_iterations as i64)
        .clamp(1, 8) as usize;
    BloomConfig {
        enabled: resolve_bool("bloom", def.enabled),
        hdr: resolve_bool("hdr", def.hdr),
        strength: resolve_f32("bloomstrength", def.strength).max(0.0),
        threshold: resolve_f32("bloomthreshold", def.threshold).clamp(0.0, 0.999),
        tint,
        hdr_strength: resolve_f32("bloomhdrstrength", def.hdr_strength).max(0.0),
        hdr_threshold: resolve_f32("bloomhdrthreshold", def.hdr_threshold).max(0.0),
        hdr_feather: resolve_f32("bloomhdrfeather", def.hdr_feather).max(0.0),
        hdr_scatter: resolve_f32("bloomhdrscatter", def.hdr_scatter).max(0.0),
        hdr_iterations,
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

pub(super) fn resolve_prop(
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

pub(super) fn value_bool(v: &serde_json::Value) -> Option<bool> {
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

pub(super) fn value_f32(v: &serde_json::Value) -> Option<f32> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::Object(o) => o.get("value").and_then(value_f32),
        _ => None,
    }
}

pub(super) fn value_vec(v: &serde_json::Value, n: usize, default: [f32; 3]) -> [f32; 3] {
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

pub(super) fn parse_color(v: &serde_json::Value, default: [f32; 3]) -> [f32; 3] {
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
// Layers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayerBlend {
    Normal,
    Additive,
    Multiply,
    Screen,
}

impl LayerBlend {
    pub(super) fn from_material(s: &str) -> Self {
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
    font_path: String,
    point_size: f32,
    spacing: [f32; 2],
    padding: [f32; 2],
    max_width: f32,
    limit_width: bool,
    limit_rows: bool,
    max_rows: usize,
    limit_use_ellipsis: bool,
    opaque_background: bool,
    background_color: [f32; 3],
    background_brightness: f32,
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

/// Per-layer authored/scripted metadata parallel to `PuppetAnimLayer`.
struct PuppetLayerMeta {
    visible_raw: Option<serde_json::Value>,
    script_visible: Option<bool>,
    blend_track: Option<KeyTrack>,
    script_blend: Option<f32>,
    rate_track: Option<KeyTrack>,
    script_rate: Option<f32>,
}

enum ObjectKind {
    Image(glow::Texture),
    Puppet {
        mesh: Mesh,
        mdl: ParsedMdl,
        anim_layers: Vec<PuppetAnimLayer>,
        anim_meta: Vec<PuppetLayerMeta>,
        texture: glow::Texture,
        tex_size: [u32; 2],
        bounds: ([f32; 2], [f32; 2]),
    },
    Text(Box<TextLayer>),
    Solid,
    Particle(usize),
    Sound(usize),
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
    solid: bool,
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
    /// Index of the parent object in `SceneRenderer::objects`, when parented.
    parent_obj: Option<usize>,
    /// Authoritative local transform components `(origin, scale, angles_rad)`.
    local_parts: ([f32; 3], [f32; 3], [f32; 3]),
    /// Index into `SceneRenderer::raw_nodes` and raw `scene.json` object definition
    /// so user property changes can re-evaluate transforms, colors, and text live.
    raw_idx: Option<usize>,
    raw_obj: serde_json::Value,
    /// Persistent visibility produced by a `visible` SceneScript (or side-effect write).
    script_visible: Option<bool>,
    /// Persistent text produced by a `text` SceneScript (or side-effect write).
    script_text_override: Option<String>,
    /// Explicit spritesheet frame set by `thisLayer.getTextureAnimation()`.
    script_sprite_frame: Option<u32>,
    /// Dynamic system or user-property texture bound via slot 0 `usertextures`.
    user_tex: Option<UserTextureSlot>,
}

fn detect_layer_user_tex(
    obj: &serde_json::Value,
    mat_pass: Option<&serde_json::Value>,
) -> Option<UserTextureSlot> {
    let slot0 = |v: &serde_json::Value| {
        v.get("usertextures")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(parse_usertexture_binding)
    };
    obj.get("instanceoverride")
        .and_then(slot0)
        .or_else(|| obj.get("instance").and_then(slot0))
        .or_else(|| mat_pass.and_then(slot0))
}

fn remap_indices_after_move(len: usize, from: usize, to: usize) -> Vec<Option<usize>> {
    let mut map: Vec<Option<usize>> = (0..len).map(Some).collect();
    if from >= len || to >= len || from == to {
        return map;
    }
    for (old, slot) in map.iter_mut().enumerate() {
        let new = if old == from {
            to
        } else if from < to && old > from && old <= to {
            old - 1
        } else if to < from && old >= to && old < from {
            old + 1
        } else {
            old
        };
        *slot = Some(new);
    }
    map
}

fn remap_indices_after_remove(len: usize, removed: usize) -> Vec<Option<usize>> {
    (0..len)
        .map(|old| {
            if old == removed {
                None
            } else if old > removed {
                Some(old - 1)
            } else {
                Some(old)
            }
        })
        .collect()
}

fn sync_media_tex_slot(
    gl: &glow::Context,
    slot: &mut Option<(u64, glow::Texture, u32, u32)>,
    art: Option<&crate::media::MediaArt>,
) {
    match art {
        None => {
            if let Some((_, tex, _, _)) = slot.take() {
                unsafe { gl.delete_texture(tex) };
            }
        }
        Some(a) => {
            if slot.is_some_and(|(rev, _, _, _)| rev == a.rev) {
                return;
            }
            if let Some((_, old_tex, _, _)) = slot.take() {
                unsafe { gl.delete_texture(old_tex) };
            }
            if let Ok(tex) = create_texture_ex(
                gl,
                a.width,
                a.height,
                &a.rgba,
                Filter::Linear,
                true,
                true,
            ) {
                *slot = Some((a.rev, tex, a.width, a.height));
            }
        }
    }
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
        origin: [f32; 3],
        scale: [f32; 3],
        angles: [f32; 3],
        props: &HashMap<String, serde_json::Value>,
        now: text::LocalTime,
    ) -> Option<Box<Self>> {
        let track = |k: &str| obj.get(k).and_then(KeyTrack::parse);
        let script = |k: &str| obj.get(k).and_then(|v| PropScript::parse(v, props));
        let a = ObjAnim {
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

    fn track(&self, field: ScriptField) -> Option<&KeyTrack> {
        match field {
            ScriptField::Origin => self.origin_track.as_ref(),
            ScriptField::Scale => self.scale_track.as_ref(),
            ScriptField::Angles => self.angles_track.as_ref(),
            ScriptField::Alpha => self.alpha_track.as_ref(),
            _ => None,
        }
    }

    fn track_mut(&mut self, field: ScriptField) -> Option<&mut KeyTrack> {
        match field {
            ScriptField::Origin => self.origin_track.as_mut(),
            ScriptField::Scale => self.scale_track.as_mut(),
            ScriptField::Angles => self.angles_track.as_mut(),
            ScriptField::Alpha => self.alpha_track.as_mut(),
            _ => None,
        }
    }

    fn preserve_playback_from(&mut self, prev: &ObjAnim) {
        if let (Some(n), Some(p)) = (&mut self.origin_track, &prev.origin_track) {
            n.preserve_playback_from(p);
        }
        if let (Some(n), Some(p)) = (&mut self.scale_track, &prev.scale_track) {
            n.preserve_playback_from(p);
        }
        if let (Some(n), Some(p)) = (&mut self.angles_track, &prev.angles_track) {
            n.preserve_playback_from(p);
        }
        if let (Some(n), Some(p)) = (&mut self.alpha_track, &prev.alpha_track) {
            n.preserve_playback_from(p);
        }
    }

    fn advance_tracks(&mut self, dt: f32) {
        if let Some(t) = &mut self.origin_track {
            t.advance(dt);
        }
        if let Some(t) = &mut self.scale_track {
            t.advance(dt);
        }
        if let Some(t) = &mut self.angles_track {
            t.advance(dt);
        }
        if let Some(t) = &mut self.alpha_track {
            t.advance(dt);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_to_parts(
        &mut self,
        parts: &mut ([f32; 3], [f32; 3], [f32; 3]),
        dt: f32,
        time: f32,
        daytime: f32,
        now: text::LocalTime,
        skip_origin_script: bool,
        skip_scale_script: bool,
        skip_angles_script: bool,
    ) -> (bool, Option<f32>) {
        let mut changed = false;
        if let Some(t) = &mut self.origin_track {
            t.advance(dt);
            parts.0 = t.apply3(self.origin);
            changed = true;
        } else if !skip_origin_script {
            if let Some(s) = &self.origin_script {
                parts.0 = s.apply(self.origin, time, daytime, now);
                changed = true;
            }
        }
        if let Some(t) = &mut self.scale_track {
            t.advance(dt);
            parts.1 = t.apply3(self.scale);
            changed = true;
        } else if !skip_scale_script {
            if let Some(s) = &self.scale_script {
                parts.1 = s.apply(self.scale, time, daytime, now);
                changed = true;
            }
        }
        if let Some(t) = &mut self.angles_track {
            t.advance(dt);
            parts.2 = t.apply3(self.angles);
            changed = true;
        } else if !skip_angles_script {
            if let Some(s) = &self.angles_script {
                // Scripts use degrees; scene.json stores radians.
                let deg = s.apply([0.0; 3], time, daytime, now);
                parts.2[2] = deg[2].to_radians();
                changed = true;
            }
        }
        let alpha = self.alpha_track.as_mut().map(|t| {
            t.advance(dt);
            t.apply3([self.base_alpha; 3])[0].clamp(0.0, 1.0)
        });
        (changed, alpha)
    }
}

impl SceneObject {
    fn is_own_visible(&self, props: &HashMap<String, serde_json::Value>) -> bool {
        self.script_visible
            .unwrap_or_else(|| self.visible.eval(props))
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

    fn hit_base_transform(&self, parallax_offset: [f32; 2]) -> Affine {
        let mut t = self.transform;
        t.tx += parallax_offset[0];
        t.ty += parallax_offset[1];
        if matches!(self.kind, ObjectKind::Puppet { .. }) {
            t
        } else {
            self.quad_transform(t)
        }
    }

    /// Map a scene world coordinate into the layer's local space.
    fn world_to_local(&self, world_pos: [f32; 2], parallax_offset: [f32; 2]) -> [f32; 2] {
        self.hit_base_transform(parallax_offset)
            .invert_point(world_pos)
            .unwrap_or([0.0, 0.0])
    }

    /// Hit-test a scene world coordinate against the layer's oriented bounds.
    fn hit_test(&self, world_pos: [f32; 2], parallax_offset: [f32; 2]) -> Option<[f32; 2]> {
        if !self.solid
            || matches!(
                self.kind,
                ObjectKind::Post | ObjectKind::Particle(_) | ObjectKind::Sound(_)
            )
        {
            return None;
        }
        let local = self
            .hit_base_transform(parallax_offset)
            .invert_point(world_pos)?;
        match &self.kind {
            ObjectKind::Puppet { bounds, .. } => {
                if local[0] >= bounds.0[0]
                    && local[0] <= bounds.1[0]
                    && local[1] >= bounds.0[1]
                    && local[1] <= bounds.1[1]
                {
                    Some(local)
                } else {
                    None
                }
            }
            _ => {
                let hw = self.size[0].abs() * 0.5;
                let hh = self.size[1].abs() * 0.5;
                if hw > 0.0 && hh > 0.0 && local[0].abs() <= hw && local[1].abs() <= hh {
                    Some(local)
                } else {
                    None
                }
            }
        }
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
            ObjectKind::Text(layer) => DrawKind::Text {
                texture: layer.texture,
                bg: layer
                    .opaque_background
                    .then_some((layer.background_color, layer.background_brightness)),
            },
            ObjectKind::Solid => DrawKind::Solid,
            ObjectKind::Particle(_)
            | ObjectKind::Sound(_)
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
        let idx = if let Some(f) = self.script_sprite_frame {
            f % frames
        } else {
            let t = (time / ss.duration.max(0.01)).rem_euclid(1.0);
            ((t * frames as f32) as u32).min(frames - 1)
        };
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
    Text {
        texture: Option<glow::Texture>,
        bg: Option<([f32; 3], f32)>,
    },
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

/// Scriptable Wallpaper Engine scene `sound` layer (lazily backed by libmpv).
struct SceneSound {
    files: Vec<std::path::PathBuf>,
    mode: super::video::AudioPlayMode,
    min_time: f32,
    max_time: f32,
    min_volume: f32,
    max_volume: f32,
    base_volume: f32,
    script_volume: Option<f32>,
    playing: bool,
    script_paused: bool,
    player: Option<super::video::AudioPlayer>,
    init_failed: bool,
}

impl SceneSound {
    fn effective_layer_volume(&self) -> f32 {
        self.script_volume
            .unwrap_or(self.base_volume)
            .clamp(0.0, 1.0)
    }

    fn is_playing(&self) -> bool {
        self.player
            .as_ref()
            .map(|p| p.is_playing())
            .unwrap_or(self.playing && !self.script_paused)
    }

    fn poll(&mut self) {
        if let Some(p) = &mut self.player {
            if p.poll() {
                self.playing = false;
            }
        }
    }

    fn sync_player(
        &mut self,
        master_volume: f32,
        muted: bool,
        global_paused: bool,
        layer_visible: bool,
        sounds_started: bool,
    ) {
        let eff_vol = (master_volume * self.effective_layer_volume()).clamp(0.0, 1.0);
        if sounds_started
            && self.player.is_none()
            && !self.init_failed
            && self.playing
            && !self.script_paused
            && eff_vol > 0.0
            && !muted
            && layer_visible
        {
            let cfg = super::video::AudioPlayerConfig {
                mode: self.mode,
                start_silent: false,
                volume: eff_vol,
                muted,
                min_time: self.min_time,
                max_time: self.max_time,
                min_volume: self.min_volume,
                max_volume: self.max_volume,
            };
            match super::video::AudioPlayer::new(&self.files, cfg) {
                Ok(p) => self.player = Some(p),
                Err(err) => {
                    log::warn!("scene sound playback disabled: {err}");
                    self.init_failed = true;
                }
            }
        }
        if let Some(p) = &mut self.player {
            p.set_volume(eff_vol);
            p.set_muted(muted || !layer_visible);
            p.set_global_paused(global_paused);
        }
    }

    fn cmd_play(
        &mut self,
        master_volume: f32,
        muted: bool,
        global_paused: bool,
        layer_visible: bool,
        sounds_started: bool,
    ) {
        self.playing = true;
        self.script_paused = false;
        if let Some(p) = &mut self.player {
            p.play();
        }
        self.sync_player(
            master_volume,
            muted,
            global_paused,
            layer_visible,
            sounds_started,
        );
    }

    fn cmd_pause(&mut self) {
        if self.playing {
            self.script_paused = true;
            if let Some(p) = &mut self.player {
                p.pause();
            }
        }
    }

    fn cmd_stop(&mut self) {
        self.playing = false;
        self.script_paused = false;
        if let Some(p) = &mut self.player {
            p.stop();
        }
    }
}

pub struct SceneRenderer {
    content_prog: Program,
    puppet_prog: Program,
    composite_prog: Program,
    blend_prog: Program,
    copy_prog: Program,
    bloom_prog: Program,
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
    bloom: BloomConfig,
    hdr_supported: bool,
    bloom_a: Option<Canvas>,
    bloom_b: Option<Canvas>,
    bloom_mips: Vec<Canvas>,

    ortho_size: [f32; 2],
    clear_color: [f32; 3],
    parallax_enabled: bool,
    parallax_amount: f32,
    parallax_mouse_influence: f32,
    parallax_delay: f32,
    camera_shake: bool,
    camera_shake_amp: f32,
    camera_shake_speed: f32,
    camera_shake_roughness: f32,
    camera_zoom: f32,
    smooth_mouse: [f32; 2],
    speed_multiplier: f32,
    /// Horizontal offset applied when a portrait scene is widened to the
    /// output aspect ratio (see `load_we_scene`).  Reset for every scene so a
    /// landscape wallpaper loaded afterwards is not shifted.
    view_shift: f32,

    objects: Vec<SceneObject>,
    emitters: Vec<Emitter>,
    /// Wallpaper Engine scene `sound` layers, lazily backed by libmpv after the
    /// first frame so mute/volume settings apply before playback starts.
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
    /// SceneScript host (QuickJS) for layers and effects with scripted properties.
    scripts: Option<SceneScripts>,
    /// Object id -> index in `objects`, for applying script output and side effects.
    script_ids: HashMap<i64, usize>,
    /// Indices of objects fed into the script host each frame.
    script_objs: Vec<usize>,
    /// Unresolved `scriptproperties` blocks keyed by binding index, so user
    /// property changes update `scriptProperties` in-place without resetting VM.
    script_binding_props: Vec<(usize, serde_json::Map<String, serde_json::Value>)>,
    /// Coalesced user property updates queued for `applyUserProperties`.
    pending_user_props: serde_json::Map<String, serde_json::Value>,
    /// Reused per-frame layer snapshot handed to the script host.
    script_frames: Vec<LayerFrame>,
    /// Set when scripts need a full rebuild (only used before scripts exist).
    scripts_dirty: bool,
    /// Layers currently hovered by the pointer for `cursorEnter`/`cursorLeave`.
    cursor_hovered: std::collections::HashSet<i64>,
    /// Layers currently holding a pointer down press for `cursorMove`/`Up`/`Click`.
    cursor_pressed: std::collections::HashSet<i64>,
    prev_mouse_down: bool,
    prev_cursor_world: Option<[f32; 2]>,
    cursor_events: Vec<CursorEventInput>,
    storage_path: std::path::PathBuf,
    pending_storage_save: Option<serde_json::Value>,
    last_storage_flush: std::time::Instant,
    media_thumb_tex: Option<(u64, glow::Texture, u32, u32)>,
    media_prev_thumb_tex: Option<(u64, glow::Texture, u32, u32)>,
    media_status_sent: bool,
    last_media_state: crate::media::MediaPlaybackState,
    last_media_props_rev: u64,
    last_media_thumb_rev: u64,
    last_media_pos: f32,
    last_media_dur: f32,
    media_events: Vec<MediaEventInput>,
    pkg: PackageFs,

    width: i32,
    height: i32,
    paused: bool,
    title: String,
}

fn sanitize_storage_key(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn fnv1a_32_hex(s: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{hash:08x}")
}

fn scene_storage_path(root: &Path, project: Option<&serde_json::Value>) -> std::path::PathBuf {
    let base = crate::util::data_dir().join("scenescript");
    let ws_raw = project
        .and_then(|p| p.get("workshopid"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
                .or_else(|| v.as_i64().filter(|&n| n > 0).map(|n| n.to_string()))
        })
        .unwrap_or_default();
    let ws_clean = sanitize_storage_key(&ws_raw);
    let key = if !ws_clean.is_empty() && ws_clean != "0" {
        ws_clean
    } else {
        let dir_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .map(sanitize_storage_key)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "scene".to_string());
        let h = fnv1a_32_hex(&root.to_string_lossy());
        format!("local_{dir_name}_{h}")
    };
    base.join(format!("{key}.json"))
}

fn tex_path_candidates(path: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    // Texture names may legitimately contain (and even end with) spaces —
    // e.g. `materials/Miles & Gwen Parts 1 .tex`. Try the raw name first,
    // then the trimmed spelling for sources that pad names.
    let raw = path.trim_start_matches('/');
    let mut names = vec![raw.to_string()];
    let trimmed = raw.trim();
    if trimmed != raw {
        names.push(trimmed.to_string());
    }
    for name in names {
        if !name.is_empty() {
            candidates.push(name.clone());
        }
        let no_ext = name
            .strip_suffix(".tex")
            .or_else(|| name.strip_suffix(".png"))
            .or_else(|| name.strip_suffix(".jpg"))
            .unwrap_or(&name);
        if !no_ext.is_empty() {
            candidates.push(format!("materials/{no_ext}.tex"));
            candidates.push(format!("{no_ext}.tex"));
        }
    }
    candidates
}

struct ResolvedModelInfo {
    model_size: Option<[f32; 2]>,
    material_blend: String,
    texture_path: Option<String>,
    puppet_path: Option<String>,
    solid_model: bool,
    mat_media_pass: Option<serde_json::Value>,
}

fn resolve_image_model(pkg: &PackageFs, image_ref: &str) -> ResolvedModelInfo {
    let mut out = ResolvedModelInfo {
        model_size: None,
        material_blend: String::from("normal"),
        texture_path: None,
        puppet_path: None,
        solid_model: false,
        mat_media_pass: None,
    };
    if !image_ref.to_ascii_lowercase().ends_with(".json") {
        out.texture_path = Some(image_ref.to_string());
        return out;
    }
    let Some(text) = read_scene_text(pkg, image_ref) else {
        return out;
    };
    let Ok(model) = serde_json::from_str::<serde_json::Value>(&text) else {
        return out;
    };
    if let Some(p) = model.get("puppet").and_then(|v| v.as_str()) {
        out.puppet_path = Some(p.to_string());
    }
    if model.get("solidlayer").and_then(value_bool) == Some(true)
        || model.get("instanced").and_then(value_bool) == Some(true)
    {
        out.solid_model = true;
    }
    if let Some(w) = model.get("width").and_then(value_f32) {
        let h = model.get("height").and_then(value_f32).unwrap_or(w);
        out.model_size = Some([w, h]);
    }
    let Some(material_ref) = model.get("material").and_then(|v| v.as_str()) else {
        return out;
    };
    let Some(mat_text) = read_scene_text(pkg, material_ref) else {
        return out;
    };
    let Ok(material) = serde_json::from_str::<serde_json::Value>(&mat_text) else {
        return out;
    };
    if let Some(pass) = material
        .get("passes")
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
    {
        if pass.get("usertextures").is_some() {
            out.mat_media_pass = Some(pass.clone());
        }
        out.material_blend = pass
            .get("blending")
            .and_then(|v| v.as_str())
            .unwrap_or("normal")
            .to_string();
        out.texture_path = pass
            .get("textures")
            .and_then(|t| t.as_array())
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    out
}

fn emitter_image_dep_id(obj: &serde_json::Value) -> Option<i64> {
    obj.get("dependencies")?
        .as_array()?
        .iter()
        .find_map(|dep| {
            let kind = dep.get("type").and_then(|v| v.as_str())?;
            if !kind.eq_ignore_ascii_case("emitterimage") {
                return None;
            }
            dep.get("id")
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        })
}

fn load_emitter_image_mask(
    pkg: &PackageFs,
    mask_obj: &serde_json::Value,
    mask_world: Affine,
    particle_world: Affine,
    props: &HashMap<String, serde_json::Value>,
) -> Option<LayerImageMask> {
    let image_ref = mask_obj.get("image").and_then(|v| v.as_str())?;
    let info = resolve_image_model(pkg, image_ref);
    let tex_ref = info.texture_path.as_deref()?;
    let decoded = tex_path_candidates(tex_ref)
        .into_iter()
        .find_map(|c| pkg.load_rgba(&c).ok())?;
    let (w, h) = (decoded.image.width(), decoded.image.height());
    let mask_size = mask_obj
        .get("size")
        .map(|v| value_vec(&resolve_prop(v, props), 3, [w as f32, h as f32, 0.0]))
        .map(|s| [s[0], s[1]])
        .or(info.model_size)
        .unwrap_or([w as f32, h as f32]);
    sample_layer_image_mask(
        decoded.image.as_raw(),
        w,
        h,
        mask_size,
        mask_world,
        particle_world,
    )
}

fn claim_cursor_press(is_hit: bool, is_visible: bool, press_claimed: &mut bool) -> bool {
    if !is_hit || *press_claimed {
        return false;
    }
    if is_visible {
        *press_claimed = true;
    }
    true
}

impl SceneRenderer {
    pub fn new(
        gl: &glow::Context,
        root: &Path,
        entry: &Path,
        width: i32,
        height: i32,
        hwdec: &str,
        project: Option<&serde_json::Value>,
    ) -> Result<Self> {
        let trace = std::env::var_os("ANIMA_TRACE_SCENE").is_some();
        let started = std::time::Instant::now();
        let pkg = PackageFs::open(root)?;
        if trace {
            log::info!("scene load: package {:.0} ms", started.elapsed().as_secs_f32() * 1000.0);
        }
        let content_prog = Program::new(gl, CONTENT_VS, CONTENT_FS)?;
        let puppet_prog = Program::new(gl, PUPPET_VS, CONTENT_FS)?;
        let composite_prog = Program::new(gl, COMPOSITE_VS, COMPOSITE_FS)?;
        let blend_prog = Program::new(gl, BLEND_VS, BLEND_FS)?;
        let copy_prog = Program::new(gl, COPY_VS, COPY_FS)?;
        let bloom_prog = Program::new(gl, BLOOM_VS, BLOOM_FS)?;
        let particle_prog = Program::new(gl, PARTICLE_VS, PARTICLE_FS)?;
        let (quad_vao, quad_vbo) = create_quad(gl)?;
        let (particle_vao, particle_quad_vbo, particle_inst_vbo) = create_particle_buffers(gl)?;
        if trace {
            log::info!("scene load: programs {:.0} ms", started.elapsed().as_secs_f32() * 1000.0);
        }

        let mut this = Self {
            content_prog,
            puppet_prog,
            composite_prog,
            blend_prog,
            copy_prog,
            bloom_prog,
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
            bloom: BloomConfig::default(),
            hdr_supported: true,
            bloom_a: None,
            bloom_b: None,
            bloom_mips: Vec::new(),
            ortho_size: [1920.0, 1080.0],
            clear_color: [0.04, 0.05, 0.08],
            parallax_enabled: false,
            parallax_amount: 0.5,
            parallax_mouse_influence: 0.4,
            parallax_delay: 0.1,
            camera_shake: false,
            camera_shake_amp: 0.01,
            camera_shake_speed: 1.5,
            camera_shake_roughness: 0.0,
            camera_zoom: 1.0,
            smooth_mouse: [0.0, 0.0],
            speed_multiplier: 1.0,
            view_shift: 0.0,
            objects: Vec::new(),
            emitters: Vec::new(),
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
            scripts: None,
            script_ids: HashMap::new(),
            script_objs: Vec::new(),
            script_binding_props: Vec::new(),
            pending_user_props: serde_json::Map::new(),
            script_frames: Vec::new(),
            scripts_dirty: false,
            cursor_hovered: std::collections::HashSet::new(),
            cursor_pressed: std::collections::HashSet::new(),
            prev_mouse_down: false,
            prev_cursor_world: None,
            cursor_events: Vec::new(),
            storage_path: scene_storage_path(root, project),
            pending_storage_save: None,
            last_storage_flush: std::time::Instant::now(),
            media_thumb_tex: None,
            media_prev_thumb_tex: None,
            media_status_sent: false,
            last_media_state: crate::media::MediaPlaybackState::Stopped,
            last_media_props_rev: 0,
            last_media_thumb_rev: 0,
            last_media_pos: 0.0,
            last_media_dur: 0.0,
            media_events: Vec::new(),
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

        // Video textures are decoded by mpv into a texture the size of an
        // output layer; configure that before any texture is loaded.
        this.textures.set_video_hint(width, height, hwdec);

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
            let t = std::time::Instant::now();
            this.load_we_scene(gl, &pkg, &json_text)?;
            if trace {
                log::info!("scene load: objects/textures {:.0} ms", t.elapsed().as_secs_f32() * 1000.0);
            }
        }

        this.pkg = pkg;
        if trace {
            log::info!("scene load: total {:.0} ms", started.elapsed().as_secs_f32() * 1000.0);
        }
        Ok(this)
    }

    fn load_texture(
        gl: &glow::Context,
        pkg: &PackageFs,
        textures: &mut TextureCache,
        path: &str,
    ) -> Option<(glow::Texture, u32, u32, Option<crate::pkg::SpritesheetMeta>)> {
        let trace = std::env::var_os("ANIMA_TRACE_SCENE").is_some();
        let started = std::time::Instant::now();
        for candidate in tex_path_candidates(path) {
            if let Some((t, w, h, ss, _)) = textures.get_spritesheet(gl, pkg, &candidate) {
                if trace {
                    let ms = started.elapsed().as_secs_f32() * 1000.0;
                    if ms > 20.0 {
                        log::info!("scene load: texture {candidate} {w}x{h} {ms:.0} ms");
                    }
                }
                return Some((t, w, h, ss));
            }
        }
        None
    }

    fn apply_general_camera_settings(&mut self, general: &serde_json::Value) {
        let resolve_f32 = |key: &str| {
            general
                .get(key)
                .map(|v| resolve_prop(v, &self.properties))
                .and_then(|v| value_f32(&v))
        };
        let resolve_bool = |key: &str| {
            general
                .get(key)
                .map(|v| resolve_prop(v, &self.properties))
                .and_then(|v| value_bool(&v))
        };
        if let Some(v) = resolve_bool("cameraparallax") {
            self.parallax_enabled = v;
        }
        if let Some(v) = resolve_f32("cameraparallaxamount") {
            self.parallax_amount = v;
        }
        if let Some(v) = resolve_f32("cameraparallaxmouseinfluence") {
            self.parallax_mouse_influence = v;
        }
        if let Some(v) = resolve_f32("cameraparallaxdelay") {
            self.parallax_delay = v.clamp(0.0, 10.0);
        }
        if let Some(v) = resolve_bool("camerashake") {
            self.camera_shake = v;
        }
        if let Some(v) = resolve_f32("camerashakeamplitude") {
            self.camera_shake_amp = v;
        }
        if let Some(v) = resolve_f32("camerashakespeed") {
            self.camera_shake_speed = v;
        }
        if let Some(v) = resolve_f32("camerashakeroughness") {
            self.camera_shake_roughness = v.clamp(0.0, 1.0);
        }
        if let Some(v) = resolve_f32("zoom") {
            self.camera_zoom = v.clamp(0.1, 10.0);
        }
    }

    fn load_we_scene(&mut self, gl: &glow::Context, pkg: &PackageFs, json_text: &str) -> Result<()> {
        let root: serde_json::Value =
            serde_json::from_str(json_text).context("parsing Wallpaper Engine scene.json")?;

        // Per-scene camera state: `view_shift` is only set by the portrait
        // widening branch below, so it must never leak in from a previous load.
        self.view_shift = 0.0;
        self.parallax_enabled = false;
        self.parallax_amount = 0.5;
        self.parallax_mouse_influence = 0.4;
        self.parallax_delay = 0.1;
        self.camera_shake = false;
        self.camera_shake_amp = 0.01;
        self.camera_shake_speed = 1.5;
        self.camera_shake_roughness = 0.0;
        self.camera_zoom = 1.0;
        self.sounds.clear();
        self.sounds_started = false;
        self.scene_general = root.get("general").cloned();
        self.raw_nodes.clear();
        self.bloom = BloomConfig::default();

        if let Some(general) = root.get("general") {
            self.bloom = parse_bloom_config(general, &self.properties);
            if let Some(ortho) = general.get("orthogonalprojection") {
                let w = ortho.get("width").and_then(value_f32).unwrap_or(1920.0);
                let h = ortho.get("height").and_then(value_f32).unwrap_or(1080.0);
                self.ortho_size = [w.max(1.0), h.max(1.0)];
            }
            if let Some(cc) = general.get("clearcolor") {
                self.clear_color = eval_color_field(Some(cc), &self.properties, [0.04, 0.05, 0.08]);
            }
            self.apply_general_camera_settings(general);
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
        let mut anims: HashMap<i64, Box<ObjAnim>> = HashMap::new();
        let mut sprites: HashMap<i64, crate::pkg::SpritesheetMeta> = HashMap::new();
        for raw in &raws {
            if let (Some(id), Some(a)) = (
                raw.obj.get("id").and_then(|v| v.as_i64()),
                ObjAnim::parse(
                    &raw.obj,
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

            let visible = own_visible[i].clone();
            let solid = obj.get("solid").and_then(value_bool).unwrap_or(true);
            let align = parse_alignment(obj);

            if let Some(particle_path) = obj
                .get("particle")
                .or_else(|| obj.get("particlesrc"))
                .and_then(|v| v.as_str())
            {
                let kind = ParticleKind::from_hint(&format!("{name} {particle_path}"));
                let preset = read_scene_text(pkg, particle_path)
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                let idx = if let Some(preset_json) = &preset {
                    let mut visited = std::collections::HashSet::new();
                    visited.insert(particle_path.to_ascii_lowercase());
                    let layer_mask = emitter_image_dep_id(obj)
                        .and_then(|dep_id| id_index.get(&dep_id.to_string()).copied())
                        .and_then(|dep_idx| {
                            load_emitter_image_mask(
                                pkg,
                                &raws[dep_idx].obj,
                                resolved[dep_idx].unwrap_or_else(Affine::identity),
                                transform,
                                &self.properties,
                            )
                        });
                    self.load_we_particle_tree(
                        gl,
                        pkg,
                        preset_json,
                        obj,
                        transform,
                        kind,
                        0,
                        &mut visited,
                        None,
                        None,
                        1.0,
                        layer_mask,
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
                    let idx = self.emitters.len();
                    self.emitters
                        .push(Emitter::new(kind, count, center, spread, color, true));
                    idx
                };
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
                    solid,
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
                    parent_obj: None,
                    local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                    script_visible: None,
                    script_text_override: None,
                    script_sprite_frame: None,
                    user_tex: None,
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
                        solid,
                        dependencies,
                        capture_composite,
                        effects: load_effects(gl, pkg, obj, &self.properties, now),
                        kind: ObjectKind::Text(Box::new(layer)),
                        anim: None,
                        sprite: None,
                        attach: None,
                        raw_idx: Some(i),
                        raw_obj: obj.clone(),
                        parent_obj: None,
                        local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                        script_visible: None,
                        script_text_override: None,
                        script_sprite_frame: None,
                        user_tex: None,
                    });
                }
                continue;
            }

            if let Some(list) = obj.get("sound").and_then(|v| v.as_array()) {
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
                if !files.is_empty() {
                    let base_volume = eval_sound_volume(obj.get("volume"), &self.properties);
                    let start_silent = obj
                        .get("startsilent")
                        .and_then(value_bool)
                        .unwrap_or(false);
                    let mode = super::video::AudioPlayMode::parse(
                        obj.get("playbackmode").and_then(|v| v.as_str()),
                    );
                    let min_time = obj
                        .get("mintime")
                        .and_then(value_f32)
                        .unwrap_or(1.0)
                        .max(0.0);
                    let max_time = obj
                        .get("maxtime")
                        .and_then(value_f32)
                        .unwrap_or(5.0)
                        .max(min_time);
                    let min_volume = obj
                        .get("minvolume")
                        .and_then(value_f32)
                        .unwrap_or(1.0)
                        .clamp(0.0, 1.0);
                    let max_volume = obj
                        .get("maxvolume")
                        .and_then(value_f32)
                        .unwrap_or(1.0)
                        .clamp(min_volume, 1.0);
                    let sidx = self.sounds.len();
                    self.sounds.push(SceneSound {
                        files,
                        mode,
                        min_time,
                        max_time,
                        min_volume,
                        max_volume,
                        base_volume,
                        script_volume: None,
                        playing: !start_silent,
                        script_paused: false,
                        player: None,
                        init_failed: false,
                    });
                    self.objects.push(SceneObject {
                        id: obj_id,
                        name,
                        transform,
                        size: [0.0, 0.0],
                        align: [0.0, 0.0],
                        color: [1.0, 1.0, 1.0, 0.0],
                        brightness: 1.0,
                        blend: LayerBlend::Normal,
                        color_blend: 0,
                        parallax: [0.0, 0.0],
                        visible,
                        solid: false,
                        dependencies,
                        capture_composite: false,
                        effects: Vec::new(),
                        kind: ObjectKind::Sound(sidx),
                        anim: None,
                        sprite: None,
                        attach: None,
                        raw_idx: Some(i),
                        raw_obj: obj.clone(),
                        parent_obj: None,
                        local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                        script_visible: None,
                        script_text_override: None,
                        script_sprite_frame: None,
                        user_tex: None,
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
                    solid,
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
                    parent_obj: None,
                    local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                    script_visible: None,
                    script_text_override: None,
                    script_sprite_frame: None,
                    user_tex: None,
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
                    solid,
                    dependencies,
                    capture_composite,
                    effects,
                    kind: ObjectKind::Compose { copy_background },
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                    parent_obj: None,
                    local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                    script_visible: None,
                    script_text_override: None,
                    script_sprite_frame: None,
                    user_tex: None,
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
                    solid,
                    dependencies,
                    capture_composite,
                    effects: load_effects(gl, pkg, obj, &self.properties, now),
                    kind: ObjectKind::Solid,
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                    parent_obj: None,
                    local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                    script_visible: None,
                    script_text_override: None,
                    script_sprite_frame: None,
                    user_tex: None,
                });
                continue;
            }

            // Model / image.
            let ResolvedModelInfo {
                model_size,
                material_blend,
                texture_path,
                puppet_path,
                solid_model,
                mat_media_pass,
            } = resolve_image_model(pkg, image_ref);
            let user_tex = detect_layer_user_tex(obj, mat_media_pass.as_ref());

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
                    solid,
                    dependencies,
                    capture_composite,
                    effects,
                    kind,
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                    parent_obj: None,
                    local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                    script_visible: None,
                    script_text_override: None,
                    script_sprite_frame: None,
                    user_tex,
                });
                continue;
            }

            let loaded = texture_path.as_deref().and_then(|tp| {
                Self::load_texture(gl, pkg, &mut self.textures, tp).map(|(t, w, h, ss)| {
                    if let (Some(ss), Some(id)) = (ss, obj_id) {
                        sprites.insert(id, ss);
                    }
                    let (w, h) = match ss {
                        Some(ss) => (w / ss.cols.max(1), h / ss.rows.max(1)),
                        None => (w, h),
                    };
                    (t, w, h)
                })
            });
            if loaded.is_none() && (user_tex.is_none() || puppet_path.is_some()) {
                if let Some(tp) = texture_path.as_deref() {
                    log::debug!("scene object {name}: missing texture {tp}");
                } else {
                    log::debug!("scene object {name}: no usable texture in {image_ref}");
                }
                continue;
            }

            let (mut default_size, kind) = if let Some((texture, tex_w, tex_h)) = loaded {
                let mut ds = model_size.unwrap_or([tex_w as f32, tex_h as f32]);
                let kind = if let Some(puppet_path) = puppet_path {
                    match pkg.read(&puppet_path).ok().and_then(|b| parse_mdl(&b)) {
                        Some(mut parsed) => {
                            let sym_bounds = [
                                (parsed.bounds_min[0].abs().max(parsed.bounds_max[0].abs()) * 2.0).max(1.0),
                                (parsed.bounds_min[1].abs().max(parsed.bounds_max[1].abs()) * 2.0).max(1.0),
                            ];
                            if model_size.is_none() {
                                ds = [
                                    ds[0].max(sym_bounds[0]),
                                    ds[1].max(sym_bounds[1]),
                                ];
                            }
                            let mut anim_layers = Vec::new();
                            let mut anim_meta = Vec::new();
                            if let Some(arr) = obj.get("animationlayers").and_then(|v| v.as_array()) {
                                for (layer_idx, layer) in arr.iter().enumerate() {
                                    let vis = parse_visible(
                                        layer.get("visible"),
                                        &self.properties,
                                        now,
                                    )
                                    .eval(&self.properties);
                                    let anim_id = layer
                                        .get("animation")
                                        .and_then(|v| v.as_i64())
                                        .map(|v| v as u32);
                                    let blend_track = layer.get("blend").and_then(KeyTrack::parse);
                                    let rate_track = layer.get("rate").and_then(KeyTrack::parse);
                                    let blend = blend_track
                                        .as_ref()
                                        .map(|t| t.eval()[0])
                                        .unwrap_or_else(|| {
                                            layer
                                                .get("blend")
                                                .map(|v| resolve_prop(v, &self.properties))
                                                .as_ref()
                                                .and_then(value_f32)
                                                .unwrap_or(1.0)
                                        })
                                        .clamp(0.0, 1.0);
                                    let rate = rate_track
                                        .as_ref()
                                        .map(|t| t.eval()[0])
                                        .unwrap_or_else(|| {
                                            layer
                                                .get("rate")
                                                .map(|v| resolve_prop(v, &self.properties))
                                                .as_ref()
                                                .and_then(value_f32)
                                                .unwrap_or(1.0)
                                        });
                                    let additive = layer
                                        .get("additive")
                                        .and_then(value_bool)
                                        .unwrap_or(false);
                                    anim_layers.push(PuppetAnimLayer {
                                        scene_idx: layer_idx,
                                        id: layer.get("id").and_then(|v| v.as_i64()),
                                        name: layer
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        anim_id,
                                        visible: vis,
                                        rate,
                                        blend,
                                        additive,
                                        playing: true,
                                        frame: 0.0,
                                        mirror_reverse: false,
                                    });
                                    anim_meta.push(PuppetLayerMeta {
                                        visible_raw: layer.get("visible").cloned(),
                                        script_visible: None,
                                        blend_track,
                                        script_blend: None,
                                        rate_track,
                                        script_rate: None,
                                    });
                                }
                            }
                            if parsed.has_animations() {
                                parsed.vertices = parsed.evaluate_vertices(0.0, &anim_layers);
                            }
                            match Mesh::new(gl, &parsed) {
                                Ok(mesh) => ObjectKind::Puppet {
                                    mesh,
                                    bounds: mdl_bounds(&parsed.vertices),
                                    mdl: parsed,
                                    anim_layers,
                                    anim_meta,
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
                (ds, kind)
            } else {
                (model_size.unwrap_or([256.0, 256.0]), ObjectKind::Solid)
            };
            let _ = &mut default_size;

            let size = obj
                .get("size")
                .map(|v| value_vec(v, 3, [default_size[0], default_size[1], 0.0]))
                .map(|v| [v[0].abs().max(1.0), v[1].abs().max(1.0)])
                .unwrap_or(default_size);

            if std::env::var_os("ANIMA_TRACE_SCENE").is_some() {
                if let (Some(tp), Some((_, tex_w, tex_h))) = (texture_path.as_deref(), loaded.as_ref()) {
                    log::info!(
                        "load obj {:?} '{}' image='{}' tex='{}' {}x{} sprite={:?}",
                        obj_id,
                        name,
                        image_ref,
                        tp,
                        tex_w,
                        tex_h,
                        obj_id.and_then(|id| sprites.get(&id).copied())
                    );
                }
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
                solid,
                dependencies,
                capture_composite,
                effects: load_effects(gl, pkg, obj, &self.properties, now),
                kind,
                anim: None,
                sprite: None,
                attach: None,
                raw_idx: Some(i),
                raw_obj: obj.clone(),
                parent_obj: None,
                local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                script_visible: None,
                script_text_override: None,
                script_sprite_frame: None,
                user_tex,
            });
        }

        // Layers that draw nothing (empty groups, effect hosts without
        // effects) are skipped above, but SceneScripts still address them
        // through `getParent()` / `thisScene.getLayer()`.  Register them as
        // zero-sized anchors so the script layer table matches the scene.
        let mut known: std::collections::HashSet<i64> =
            self.objects.iter().filter_map(|o| o.id).collect();
        for (i, raw) in raws.iter().enumerate() {
            let obj = &raw.obj;
            let Some(id) = obj
                .get("id")
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            else {
                continue;
            };
            if !known.insert(id) {
                continue;
            }
            self.objects.push(SceneObject {
                id: Some(id),
                name: obj
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("object")
                    .to_string(),
                transform: resolved[i].unwrap_or_else(Affine::identity),
                size: [0.0, 0.0],
                align: parse_alignment(obj),
                color: [
                    1.0,
                    1.0,
                    1.0,
                    eval_alpha_field(obj.get("alpha"), &self.properties, now),
                ],
                brightness: 1.0,
                blend: LayerBlend::Normal,
                color_blend: 0,
                parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                visible: own_visible[i].clone(),
                solid: obj.get("solid").and_then(value_bool).unwrap_or(true),
                dependencies: Vec::new(),
                capture_composite: false,
                effects: Vec::new(),
                kind: ObjectKind::Solid,
                anim: None,
                sprite: None,
                attach: None,
                raw_idx: Some(i),
                raw_obj: obj.clone(),
                parent_obj: None,
                local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                script_visible: None,
                script_text_override: None,
                script_sprite_frame: None,
                user_tex: None,
            });
        }

        // Wire object parent indices and authoritative local transform parts
        // across all objects (including zero-sized anchors).
        let mut id_to_obj: HashMap<i64, usize> = HashMap::with_capacity(self.objects.len());
        for (j, o) in self.objects.iter().enumerate() {
            if let Some(id) = o.id {
                id_to_obj.insert(id, j);
            }
        }
        for j in 0..self.objects.len() {
            if let Some(pid) = self.objects[j].raw_obj.get("parent").and_then(parent_id) {
                if let Some(&p_idx) = id_to_obj.get(&pid) {
                    if p_idx != j {
                        self.objects[j].parent_obj = Some(p_idx);
                    }
                }
            }
            if let Some(ri) = self.objects[j].raw_idx {
                if let Some(raw) = raws.get(ri) {
                    let has_scale_dyn = raw.obj.get("scale").is_some_and(|v| {
                        v.get("script").and_then(|s| s.as_str()).is_some()
                            || v.get("animation").is_some()
                    });
                    let eff_scale = if has_scale_dyn {
                        raw.scale
                    } else {
                        [
                            if raw.scale[0] == 0.0 { 1.0 } else { raw.scale[0] },
                            if raw.scale[1] == 0.0 { 1.0 } else { raw.scale[1] },
                            raw.scale[2],
                        ]
                    };
                    self.objects[j].local_parts = (raw.origin, eff_scale, raw.angles);
                }
            }
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
            if matches!(
                o.kind,
                ObjectKind::Particle(_) | ObjectKind::Post | ObjectKind::Sound(_)
            ) {
                continue;
            }
            if matches!(o.kind, ObjectKind::Image(_)) {
                o.sprite = sprites.get(&id).copied();
            }
            if let Some(a) = anims.remove(&id) {
                o.anim = Some(a);
            }
        }
        self.load_scene_scripts(gl, Some(pkg));
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
                    ObjectKind::Sound(_) => "sound",
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
                    self.is_obj_visible(i)
                );
            }
        }
        Ok(())
    }

    /// Whether object `idx` and every ancestor in its parent chain are visible.
    fn is_obj_visible(&self, idx: usize) -> bool {
        let mut cur = Some(idx);
        for _ in 0..32 {
            let Some(i) = cur else { break };
            let obj = &self.objects[i];
            if !obj.is_own_visible(&self.properties) {
                return false;
            }
            cur = obj.parent_obj;
        }
        true
    }

    /// Recompute world transforms across the object hierarchy from `local_parts`.
    fn propagate_transforms(&mut self) {
        let n = self.objects.len();
        if n == 0 {
            return;
        }
        let root = Affine::translate(self.view_shift, 0.0);
        let mut world: Vec<Option<Affine>> = vec![None; n];
        for i in 0..n {
            let mut chain = [0usize; 32];
            let mut depth = 0usize;
            let mut cur = Some(i);
            let mut base = root;
            while let Some(idx) = cur {
                if let Some(w) = world[idx] {
                    base = w;
                    break;
                }
                if depth >= chain.len() {
                    break;
                }
                chain[depth] = idx;
                depth += 1;
                cur = self.objects[idx].parent_obj;
            }
            let mut acc = base;
            for &idx in chain[..depth].iter().rev() {
                let obj = &self.objects[idx];
                if matches!(obj.kind, ObjectKind::Post) || obj.attach.is_some() {
                    acc = obj.transform;
                } else {
                    let (origin, scale, angles) = obj.local_parts;
                    acc = acc.mul(compose_local(origin, scale, angles));
                }
                world[idx] = Some(acc);
            }
        }
        let mut emitter_roots: Vec<(usize, Affine)> = Vec::new();
        for (i, obj) in self.objects.iter_mut().enumerate() {
            if matches!(obj.kind, ObjectKind::Post) || obj.attach.is_some() {
                continue;
            }
            if let Some(w) = world[i] {
                obj.transform = w;
                if let ObjectKind::Particle(pidx) = obj.kind {
                    emitter_roots.push((pidx, w));
                }
            }
        }
        for (pidx, w) in emitter_roots {
            self.sync_emitter_tree_transform(pidx, w);
        }
    }

    fn sync_emitter_tree_transform(&mut self, idx: usize, world: Affine) {
        let Some(em) = self.emitters.get_mut(idx) else {
            return;
        };
        em.center = [world.tx, world.ty];
        if let Some(spec) = &mut em.we_spec {
            spec.transform = world;
        }
        let children = em.child_indices.clone();
        for (cidx, _) in children {
            let local = self
                .emitters
                .get(cidx)
                .and_then(|c| c.child_local)
                .unwrap_or_else(Affine::identity);
            self.sync_emitter_tree_transform(cidx, world.mul(local));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn load_we_particle_tree(
        &mut self,
        gl: &glow::Context,
        pkg: &PackageFs,
        preset_json: &serde_json::Value,
        obj: &serde_json::Value,
        transform: Affine,
        hint_kind: ParticleKind,
        depth: usize,
        visited: &mut std::collections::HashSet<String>,
        child_local: Option<Affine>,
        child_maxcount: Option<usize>,
        child_prob: f32,
        layer_mask: Option<LayerImageMask>,
    ) -> usize {
        let emitter = Emitter::from_we_preset(
            gl,
            pkg,
            &mut self.textures,
            preset_json,
            obj,
            transform,
            hint_kind,
            &self.properties,
            child_local,
            child_maxcount,
            child_prob,
            layer_mask,
        );
        let parent_idx = self.emitters.len();
        self.emitters.push(emitter);
        if depth >= 3 {
            return parent_idx;
        }
        let Some(children) = preset_json.get("children").and_then(|c| c.as_array()) else {
            return parent_idx;
        };
        for ch in children {
            let Some(child_path) = ch.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            let key = child_path.to_ascii_lowercase();
            if visited.contains(&key) {
                continue;
            }
            let Some(child_preset) = read_scene_text(pkg, child_path)
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            else {
                continue;
            };
            let origin = ch
                .get("origin")
                .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0]))
                .unwrap_or([0.0, 0.0, 0.0]);
            let scale = ch
                .get("scale")
                .map(|v| value_vec(v, 3, [1.0, 1.0, 1.0]))
                .unwrap_or([1.0, 1.0, 1.0]);
            let angles = ch
                .get("angles")
                .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0]))
                .unwrap_or([0.0, 0.0, 0.0]);
            let local = compose_local(origin, scale, angles);
            let child_world = transform.mul(local);
            let mode = match ch
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("static")
                .to_ascii_lowercase()
                .as_str()
            {
                "eventfollow" => ChildSpawnMode::EventFollow,
                "eventspawn" => ChildSpawnMode::EventSpawn,
                _ => ChildSpawnMode::Static,
            };
            let maxc = ch
                .get("maxcount")
                .and_then(value_f32)
                .map(|f| (f.round() as usize).clamp(1, 500));
            let prob = ch.get("probability").and_then(value_f32).unwrap_or(1.0);
            let child_hint = ParticleKind::from_hint(child_path);

            visited.insert(key.clone());
            let child_idx = self.load_we_particle_tree(
                gl,
                pkg,
                &child_preset,
                obj,
                child_world,
                child_hint,
                depth + 1,
                visited,
                Some(local),
                maxc,
                prob,
                None,
            );
            visited.remove(&key);
            self.emitters[parent_idx].child_indices.push((child_idx, mode));
        }
        parent_idx
    }

    fn reset_emitter_tree_overrides(emitters: &mut [Emitter], idx: usize) {
        let Some(em) = emitters.get_mut(idx) else {
            return;
        };
        em.reset_script_overrides();
        let children = em.child_indices.clone();
        for (cidx, _) in children {
            Self::reset_emitter_tree_overrides(emitters, cidx);
        }
    }

    fn update_emitter_tree_override(&mut self, idx: usize, key: &str, value: &serde_json::Value) {
        let Some(em) = self.emitters.get_mut(idx) else {
            return;
        };
        em.update_instance_override(key, value);
        let children = em.child_indices.clone();
        for (cidx, _) in children {
            self.update_emitter_tree_override(cidx, key, value);
        }
    }

    fn update_emitter_tree_color(emitters: &mut [Emitter], idx: usize, color_mult: [f32; 3]) {
        let Some(em) = emitters.get_mut(idx) else {
            return;
        };
        em.update_color_mult(color_mult);
        let children = em.child_indices.clone();
        for (cidx, _) in children {
            Self::update_emitter_tree_color(emitters, cidx, color_mult);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_emitter_tree(
        &mut self,
        gl: &glow::Context,
        idx: usize,
        dt: f32,
        time: f32,
        ortho: [f32; 2],
        audio: [f32; 4],
        spectrum: &[f32; 64],
        offset: [f32; 2],
        pointer_world: [f32; 2],
        spawn_anchors: Option<(&[[f32; 2]], bool)>,
    ) -> Result<()> {
        if idx >= self.emitters.len() {
            return Ok(());
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

        let children = self.emitters[idx].child_indices.clone();
        let need_anchors = children.iter().any(|(_, m)| *m != ChildSpawnMode::Static);

        let emitter = &mut self.emitters[idx];
        apply_blend(gl, emitter.blend);
        emitter.update_and_pack(
            dt,
            time,
            ortho,
            audio,
            spectrum,
            offset,
            pointer_world,
            spawn_anchors,
            &mut self.instance_scratch,
        );

        if !self.instance_scratch.is_empty() {
            unsafe {
                let p = &mut self.particle_prog;
                p.bind(gl);
                p.set_vec2(gl, "u_ortho", ortho[0], ortho[1]);
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
        }

        if children.is_empty() {
            return Ok(());
        }

        let world_anchors: Vec<[f32; 2]> = if need_anchors {
            let em = &self.emitters[idx];
            let step = (em.particles.len() / 64).max(1);
            if let Some(spec) = &em.we_spec {
                em.particles
                    .iter()
                    .step_by(step)
                    .take(64)
                    .map(|p| spec.transform.apply([p.x, p.y]))
                    .collect()
            } else {
                let sx = em.spread[0].max(1.0);
                let sy = em.spread[1].max(1.0);
                em.particles
                    .iter()
                    .step_by(step)
                    .take(64)
                    .map(|p| [em.center[0] + p.x * sx, em.center[1] + p.y * sy])
                    .collect()
            }
        } else {
            Vec::new()
        };

        for (cidx, mode) in children {
            if mode == ChildSpawnMode::Static || world_anchors.is_empty() {
                self.draw_emitter_tree(
                    gl,
                    cidx,
                    dt,
                    time,
                    ortho,
                    audio,
                    spectrum,
                    offset,
                    pointer_world,
                    None,
                )?;
            } else {
                let child_xform = self
                    .emitters
                    .get(cidx)
                    .and_then(|c| c.we_spec.as_ref().map(|s| s.transform))
                    .unwrap_or_else(Affine::identity);
                let local_anchors: Vec<[f32; 2]> = world_anchors
                    .iter()
                    .filter_map(|&w| child_xform.invert_point(w))
                    .collect();
                let follow = mode == ChildSpawnMode::EventFollow;
                let anchors_opt = (!local_anchors.is_empty()).then_some((local_anchors.as_slice(), follow));
                self.draw_emitter_tree(
                    gl,
                    cidx,
                    dt,
                    time,
                    ortho,
                    audio,
                    spectrum,
                    offset,
                    pointer_world,
                    anchors_opt,
                )?;
            }
        }
        Ok(())
    }

    /// Compile every SceneScript bound to a layer or effect property and register
    /// the layer/effect tables the scripts see through `thisLayer`/`thisObject`/`thisScene`.
    fn load_scene_scripts(&mut self, gl: &glow::Context, pkg_override: Option<&PackageFs>) {
        self.capture_and_flush_storage(true);
        self.scripts_dirty = false;
        self.scripts = None;
        self.script_ids.clear();
        self.script_objs.clear();
        self.script_binding_props.clear();
        self.pending_user_props.clear();
        self.script_frames.clear();
        self.cursor_hovered.clear();
        self.cursor_pressed.clear();
        self.prev_mouse_down = false;
        self.prev_cursor_world = None;
        self.cursor_events.clear();
        self.media_status_sent = false;
        for snd in &mut self.sounds {
            snd.script_volume = None;
        }
        for obj in self.objects.iter_mut() {
            obj.script_visible = None;
            obj.script_text_override = None;
            obj.script_sprite_frame = None;
            for eff in &mut obj.effects {
                eff.clear_script_state();
            }
            match &mut obj.kind {
                ObjectKind::Puppet { anim_meta, .. } => {
                    for meta in anim_meta {
                        meta.script_visible = None;
                        meta.script_blend = None;
                        meta.script_rate = None;
                    }
                }
                ObjectKind::Particle(pidx) => {
                    Self::reset_emitter_tree_overrides(&mut self.emitters, *pidx);
                }
                _ => {}
            }
        }

        if self.raw_nodes.is_empty()
            || !self
                .objects
                .iter()
                .any(|o| !o.raw_obj.is_null() && object_has_script(&o.raw_obj))
        {
            return;
        }

        let user_props: serde_json::Map<String, serde_json::Value> = self
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let saved_storage: Option<serde_json::Value> = if self.storage_path.is_file() {
            let val: serde_json::Value = crate::util::read_json_or_backup(&self.storage_path);
            val.is_object().then_some(val)
        } else {
            None
        };
        let Some(mut host) = SceneScripts::new_with_storage(&user_props, saved_storage.as_ref())
        else {
            log::warn!("scene: SceneScript runtime unavailable");
            return;
        };

        // Layers whose live values have to reach the scripts: the scripted
        // layers themselves, their parents (`getParent()`), and layers a
        // script addresses by name (or all layers when `enumerateLayers` is used).
        let mut feed: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut named: Vec<String> = Vec::new();
        let mut enumerates_all = false;
        for (idx, obj) in self.objects.iter().enumerate() {
            if obj.raw_obj.is_null() || !object_has_script(&obj.raw_obj) {
                continue;
            }
            feed.insert(idx);
            named.extend(layer_script_refs(&obj.raw_obj));
            if object_enumerates_layers(&obj.raw_obj) {
                enumerates_all = true;
            }
            let mut cur = obj.parent_obj;
            for _ in 0..64 {
                let Some(p) = cur else { break };
                if !feed.insert(p) {
                    break;
                }
                cur = self.objects[p].parent_obj;
            }
        }
        if enumerates_all {
            for (i, obj) in self.objects.iter().enumerate() {
                if obj.id.is_some() {
                    feed.insert(i);
                }
            }
        } else {
            for name in named {
                for (i, obj) in self.objects.iter().enumerate() {
                    if obj.name == name {
                        feed.insert(i);
                    }
                }
            }
        }

        let now = text::local_time();
        let mut registered_ids: std::collections::HashSet<i64> =
            std::collections::HashSet::new();
        for (idx, obj) in self.objects.iter().enumerate() {
            let Some(id) = obj.id else {
                continue;
            };
            registered_ids.insert(id);
            let (origin, scale, angles) = obj.local_parts;
            let kind = match &obj.kind {
                ObjectKind::Image(_) => "image",
                ObjectKind::Puppet { .. } => "puppet",
                ObjectKind::Text(_) => "text",
                ObjectKind::Solid => "solid",
                ObjectKind::Particle(_) => "particle",
                ObjectKind::Sound(_) => "sound",
                ObjectKind::Compose { .. } | ObjectKind::CompositeRef(_) | ObjectKind::Post => {
                    "group"
                }
            };
            let text = match &obj.kind {
                ObjectKind::Text(layer) => layer.evaluate(&self.properties, now),
                _ => String::new(),
            };
            let parent = obj.raw_obj.get("parent").and_then(parent_id);
            let tex_anim = obj.sprite.map(|ss| TexAnimSeed {
                frames: ss.frames.max(1),
                duration: ss.duration.max(0.01),
            });
            let video_tex = match &obj.kind {
                ObjectKind::Image(tex) => self.textures.video_by_texture(*tex).map(|vt| VideoTexSeed {
                    duration: vt.duration() as f32,
                    current_time: vt.current_time() as f32,
                    playing: !vt.is_script_paused(),
                    rate: vt.playback_rate(),
                }),
                _ => None,
            };
            let sound = match &obj.kind {
                ObjectKind::Sound(sidx) => self.sounds.get(*sidx).map(|s| SoundSeed {
                    volume: s.effective_layer_volume(),
                    playing: s.is_playing(),
                }),
                _ => None,
            };
            let authored_align = obj
                .raw_obj
                .get("alignment")
                .and_then(|v| v.as_str())
                .unwrap_or("center")
                .to_string();
            let (horizontal_align, vertical_align, point_size, font) = match &obj.kind {
                ObjectKind::Text(layer) => (
                    match layer.h_align {
                        text::HAlign::Left => "left",
                        text::HAlign::Right => "right",
                        text::HAlign::Center => "center",
                    }
                    .to_string(),
                    match layer.v_align {
                        text::VAlign::Top => "top",
                        text::VAlign::Bottom => "bottom",
                        text::VAlign::Center => "center",
                    }
                    .to_string(),
                    layer.point_size,
                    layer.font_path.clone(),
                ),
                _ => ("center".to_string(), "center".to_string(), 32.0, String::new()),
            };
            let authored_image = obj
                .raw_obj
                .get("image")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            host.add_layer(&LayerSeed {
                id,
                name: obj.name.clone(),
                kind: kind.to_string(),
                visible: obj.is_own_visible(&self.properties),
                solid: obj.solid,
                alpha: obj.color[3],
                origin,
                scale,
                angles: [
                    angles[0].to_degrees(),
                    angles[1].to_degrees(),
                    angles[2].to_degrees(),
                ],
                color: [obj.color[0], obj.color[1], obj.color[2]],
                size: obj.size,
                align: authored_align,
                horizontal_align,
                vertical_align,
                point_size,
                font,
                image: authored_image,
                text,
                parent,
                tex_anim,
                video_tex,
                sound,
            });
            for eff in &obj.effects {
                let raw_eff = obj
                    .raw_obj
                    .get("effects")
                    .and_then(|e| e.as_array())
                    .and_then(|a| a.get(eff.scene_idx));
                let vis_has_script = raw_eff
                    .and_then(|re| re.get("visible"))
                    .and_then(|v| v.get("script"))
                    .and_then(|s| s.as_str())
                    .is_some();
                let eff_vis = if vis_has_script {
                    raw_bool_field(raw_eff.and_then(|re| re.get("visible")), &self.properties)
                } else {
                    parse_visible(eff.visible_raw.as_ref(), &self.properties, now)
                        .eval(&self.properties)
                };
                host.add_effect(&EffectSeed {
                    layer_id: id,
                    effect_idx: eff.scene_idx,
                    id: eff.id,
                    name: eff.name.clone(),
                    visible: eff_vis,
                });
            }
            if let ObjectKind::Puppet {
                mdl, anim_layers, ..
            } = &obj.kind
            {
                let raw_als = obj
                    .raw_obj
                    .get("animationlayers")
                    .and_then(|v| v.as_array());
                for al in anim_layers {
                    let raw_al = raw_als.and_then(|arr| arr.get(al.scene_idx));
                    let vis_has_script = raw_al
                        .and_then(|ra| ra.get("visible"))
                        .and_then(|v| v.get("script"))
                        .and_then(|s| s.as_str())
                        .is_some();
                    let al_vis = if vis_has_script {
                        raw_bool_field(raw_al.and_then(|ra| ra.get("visible")), &self.properties)
                    } else {
                        al.visible
                    };
                    let (frame_count, fps, duration, mode) = match mdl.find_anim(al.anim_id) {
                        Some(anim) => {
                            let fc = mdl.anim_frame_count(anim) as u32;
                            let fps = anim.fps.max(1.0);
                            let dur = (fc.saturating_sub(1) as f32) / fps;
                            (fc, fps, dur, anim.mode.as_str())
                        }
                        None => (1, 30.0, 0.0, "loop"),
                    };
                    host.add_anim_layer(&AnimLayerSeed {
                        layer_id: id,
                        anim_layer_idx: al.scene_idx,
                        id: al.id,
                        name: al.name.clone(),
                        visible: al_vis,
                        blend: al.blend,
                        rate: al.rate,
                        frame_count,
                        fps,
                        duration,
                        mode,
                    });
                }
            }
            // Any registered object can be mutated via side effects (`getLayer`, `enumerateLayers`).
            self.script_ids.insert(id, idx);
            if feed.contains(&idx) {
                self.script_objs.push(idx);
            }
        }

        host.link_parents();
        if log::log_enabled!(log::Level::Debug) {
            let missing: Vec<String> = self
                .script_objs
                .iter()
                .filter_map(|&idx| {
                    let o = &self.objects[idx];
                    let p = o.raw_obj.get("parent").and_then(parent_id)?;
                    (!registered_ids.contains(&p)).then(|| format!("{}->{p}", o.id.unwrap_or(-1)))
                })
                .take(8)
                .collect();
            if !missing.is_empty() {
                log::debug!("scene: SceneScript parents not registered: {missing:?}");
            }
        }

        let mut bindings = 0usize;
        let mut binding_layers = 0usize;
        for obj in &self.objects {
            let Some(id) = obj.id else { continue };
            if obj.raw_obj.is_null() {
                continue;
            }
            let raw = &obj.raw_obj;
            let mut has = false;
            for (field, key) in [
                (ScriptField::Visible, "visible"),
                (ScriptField::Alpha, "alpha"),
                (ScriptField::Origin, "origin"),
                (ScriptField::Scale, "scale"),
                (ScriptField::Angles, "angles"),
                (ScriptField::Color, "color"),
                (ScriptField::Text, "text"),
                (ScriptField::Volume, "volume"),
            ] {
                let Some(v) = raw.get(key) else { continue };
                let Some(script) = v.get("script").and_then(|s| s.as_str()) else {
                    continue;
                };
                let (initial, script_props) = field_script_defaults(field, v, &self.properties);
                let prop_anim = obj
                    .anim
                    .as_deref()
                    .and_then(|a| a.track(field))
                    .map(PropAnimSeed::from_track);
                let bidx = host.add_property_script(
                    id,
                    field,
                    script,
                    initial,
                    &script_props,
                    prop_anim.as_ref(),
                );
                if let Some(raw_sp) = v.get("scriptproperties").and_then(|p| p.as_object()) {
                    if !raw_sp.is_empty() {
                        self.script_binding_props.push((bidx, raw_sp.clone()));
                    }
                }
                bindings += 1;
                has = true;
            }

            if let Some(raw_effects) = raw.get("effects").and_then(|e| e.as_array()) {
                for eff in &obj.effects {
                    let Some(raw_eff) = raw_effects.get(eff.scene_idx) else {
                        continue;
                    };
                    if let Some(vis) = raw_eff.get("visible") {
                        if let Some(script) = vis.get("script").and_then(|s| s.as_str()) {
                            let initial = raw_bool_field(Some(vis), &self.properties);
                            let raw_sp = vis.get("scriptproperties").and_then(|p| p.as_object());
                            let script_props = resolve_script_props(raw_sp, &self.properties);
                            let bidx = host.add_effect_visible_script(
                                id,
                                eff.scene_idx,
                                script,
                                initial,
                                &script_props,
                            );
                            if let Some(sp) = raw_sp {
                                if !sp.is_empty() {
                                    self.script_binding_props.push((bidx, sp.clone()));
                                }
                            }
                            bindings += 1;
                            has = true;
                        }
                    }
                    if let Some(passes) = raw_eff.get("passes").and_then(|p| p.as_array()) {
                        for (pass_idx, raw_pass) in passes.iter().enumerate() {
                            let Some(csv) = raw_pass
                                .get("constantshadervalues")
                                .and_then(|c| c.as_object())
                            else {
                                continue;
                            };
                            for (ck, cv) in csv {
                                let Some(script) = cv.get("script").and_then(|s| s.as_str()) else {
                                    continue;
                                };
                                let initial = if cv.get("user").is_some() {
                                    let r = resolve_prop(cv, &self.properties);
                                    r.get("value").cloned().unwrap_or(r)
                                } else if let Some(v) = cv.get("value") {
                                    let r = resolve_prop(v, &self.properties);
                                    r.get("value").cloned().unwrap_or(r)
                                } else if let Some(def) = eff.constant_initial_value(pass_idx, ck) {
                                    let r = resolve_prop(&def, &self.properties);
                                    r.get("value").cloned().unwrap_or(r)
                                } else {
                                    serde_json::json!(0.0)
                                };
                                let raw_sp = cv.get("scriptproperties").and_then(|p| p.as_object());
                                let script_props = resolve_script_props(raw_sp, &self.properties);
                                let prop_anim = eff
                                    .constant_track(pass_idx, ck)
                                    .map(PropAnimSeed::from_track);
                                let bidx = host.add_effect_constant_script(
                                    id,
                                    eff.scene_idx,
                                    pass_idx,
                                    ck,
                                    script,
                                    initial,
                                    &script_props,
                                    prop_anim.as_ref(),
                                );
                                if let Some(sp) = raw_sp {
                                    if !sp.is_empty() {
                                        self.script_binding_props.push((bidx, sp.clone()));
                                    }
                                }
                                bindings += 1;
                                has = true;
                            }
                        }
                    }
                }
            }
            if let (ObjectKind::Puppet { anim_layers, .. }, Some(raw_als)) = (
                &obj.kind,
                raw.get("animationlayers").and_then(|v| v.as_array()),
            ) {
                for al in anim_layers {
                    let Some(raw_al) = raw_als.get(al.scene_idx) else {
                        continue;
                    };
                    for (field, key) in [
                        (AnimLayerField::Visible, "visible"),
                        (AnimLayerField::Blend, "blend"),
                        (AnimLayerField::Rate, "rate"),
                    ] {
                        let Some(v) = raw_al.get(key) else {
                            continue;
                        };
                        let Some(script) = v.get("script").and_then(|s| s.as_str()) else {
                            continue;
                        };
                        let initial = match field {
                            AnimLayerField::Visible => {
                                serde_json::Value::Bool(raw_bool_field(Some(v), &self.properties))
                            }
                            AnimLayerField::Blend => serde_json::Value::from(
                                value_f32(&resolve_prop(v, &self.properties)).unwrap_or(al.blend),
                            ),
                            AnimLayerField::Rate => serde_json::Value::from(
                                value_f32(&resolve_prop(v, &self.properties)).unwrap_or(al.rate),
                            ),
                        };
                        let raw_sp = v.get("scriptproperties").and_then(|p| p.as_object());
                        let script_props = resolve_script_props(raw_sp, &self.properties);
                        let bidx = host.add_anim_layer_script(
                            id,
                            al.scene_idx,
                            field,
                            script,
                            initial,
                            &script_props,
                            None,
                        );
                        if let Some(sp) = raw_sp {
                            if !sp.is_empty() {
                                self.script_binding_props.push((bidx, sp.clone()));
                            }
                        }
                        bindings += 1;
                        has = true;
                    }
                }
            }
            if let Some(inst) = raw.get("instanceoverride").and_then(|v| v.as_object()) {
                for (ok, ov) in inst {
                    let Some(script) = ov.get("script").and_then(|s| s.as_str()) else {
                        continue;
                    };
                    let key_lower = ok.to_ascii_lowercase();
                    let initial = if ov.get("user").is_some() {
                        let r = resolve_prop(ov, &self.properties);
                        r.get("value").cloned().unwrap_or(r)
                    } else if let Some(v) = ov.get("value") {
                        let r = resolve_prop(v, &self.properties);
                        r.get("value").cloned().unwrap_or(r)
                    } else if matches!(key_lower.as_str(), "color" | "colorn") {
                        serde_json::json!([1.0, 1.0, 1.0])
                    } else {
                        serde_json::json!(1.0)
                    };
                    let raw_sp = ov.get("scriptproperties").and_then(|p| p.as_object());
                    let script_props = resolve_script_props(raw_sp, &self.properties);
                    let bidx =
                        host.add_instance_override_script(id, ok, script, initial, &script_props);
                    if let Some(sp) = raw_sp {
                        if !sp.is_empty() {
                            self.script_binding_props.push((bidx, sp.clone()));
                        }
                    }
                    bindings += 1;
                    has = true;
                }
            }
            if has {
                binding_layers += 1;
            }
        }

        if bindings > 0 {
            let init_outputs = host.finalize_init();
            let failing = host.failures();
            if failing > 0 {
                for err in host.errors().iter().take(4) {
                    log::warn!("scene: SceneScript: {err}");
                }
            }
            log::debug!(
                "scene: SceneScript {bindings} binding(s) on {binding_layers} layer(s), audio={}, {failing} failing",
                host.uses_audio()
            );
            self.scripts = Some(host);
            self.apply_script_outputs(gl, pkg_override, init_outputs);
            self.capture_and_flush_storage(false);
        }
    }

    fn capture_and_flush_storage(&mut self, force: bool) {
        if let Some(snapshot) = self
            .scripts
            .as_mut()
            .and_then(|h| h.take_storage_update())
        {
            self.pending_storage_save = Some(snapshot);
        }
        if self.pending_storage_save.is_none() {
            return;
        }
        if !force && self.last_storage_flush.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        if let Some(snapshot) = self.pending_storage_save.take() {
            match serde_json::to_vec_pretty(&snapshot) {
                Ok(bytes) => {
                    if let Err(err) = crate::util::atomic_write(&self.storage_path, &bytes) {
                        log::warn!(
                            "scene: cannot persist storage {}: {err}",
                            self.storage_path.display()
                        );
                        self.pending_storage_save = Some(snapshot);
                    } else {
                        self.last_storage_flush = std::time::Instant::now();
                    }
                }
                Err(err) => log::warn!("scene: cannot serialize storage: {err}"),
            }
        }
    }

    fn apply_object_index_remap(&mut self, old_to_new: &[Option<usize>]) {
        for obj in &mut self.objects {
            if let Some(p) = obj.parent_obj {
                obj.parent_obj = old_to_new.get(p).copied().flatten();
            }
            if let Some(att) = &mut obj.attach {
                match old_to_new.get(att.parent).copied().flatten() {
                    Some(np) => att.parent = np,
                    None => obj.attach = None,
                }
            }
        }
        let mut remapped_scripts = Vec::with_capacity(self.script_objs.len());
        for &old_idx in &self.script_objs {
            if let Some(new_idx) = old_to_new.get(old_idx).copied().flatten() {
                remapped_scripts.push(new_idx);
            }
        }
        self.script_objs = remapped_scripts;
        self.script_ids.clear();
        for (idx, obj) in self.objects.iter().enumerate() {
            if let Some(id) = obj.id {
                self.script_ids.insert(id, idx);
            }
        }
    }

    fn create_dynamic_layer(
        &mut self,
        gl: &glow::Context,
        pkg_override: Option<&PackageFs>,
        id: i64,
        cfg: &serde_json::Value,
    ) -> bool {
        if self.objects.len() >= 1024 || self.script_ids.contains_key(&id) {
            return false;
        }
        let name = cfg
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("dynamic")
            .to_string();
        let kind_str = cfg
            .get("kind")
            .or_else(|| cfg.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let image_ref = cfg
            .get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let origin = cfg
            .get("origin")
            .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0]))
            .unwrap_or([0.0, 0.0, 0.0]);
        let scale = cfg
            .get("scale")
            .map(|v| value_vec(v, 3, [1.0, 1.0, 1.0]))
            .unwrap_or([1.0, 1.0, 1.0]);
        let angles_deg = cfg
            .get("angles")
            .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0]))
            .unwrap_or([0.0, 0.0, 0.0]);
        let angles = [
            angles_deg[0].to_radians(),
            angles_deg[1].to_radians(),
            angles_deg[2].to_radians(),
        ];
        let local = compose_local(origin, scale, angles);
        let color_rgb = cfg
            .get("color")
            .map(|v| parse_color(v, [1.0, 1.0, 1.0]))
            .unwrap_or([1.0, 1.0, 1.0]);
        let alpha = cfg
            .get("alpha")
            .and_then(value_f32)
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        let visible = cfg.get("visible").and_then(value_bool).unwrap_or(true);
        let solid = cfg.get("solid").and_then(value_bool).unwrap_or(true);
        let align = parse_alignment_str(
            cfg.get("alignment")
                .and_then(|v| v.as_str())
                .unwrap_or("center"),
        );
        let parallax = cfg
            .get("parallaxDepth")
            .map(|v| {
                let p = value_vec(v, 2, [0.0, 0.0, 0.0]);
                [p[0], p[1]]
            })
            .unwrap_or([0.0, 0.0]);
        let parent_obj = cfg
            .get("parent")
            .and_then(|v| v.as_i64())
            .and_then(|pid| self.script_ids.get(&pid).copied());
        let raw_size = cfg
            .get("size")
            .map(|v| {
                let s = value_vec(v, 2, [0.0, 0.0, 0.0]);
                [s[0].abs(), s[1].abs()]
            })
            .unwrap_or([0.0, 0.0]);

        let pkg = pkg_override.unwrap_or(&self.pkg);
        let has_text_field = cfg
            .get("text")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        let (kind, size, blend, sprite) = if (kind_str == "text" || has_text_field)
            && kind_str != "solid"
        {
            let authored = cfg
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let point_size = cfg
                .get("pointsize")
                .and_then(value_f32)
                .unwrap_or(32.0)
                .clamp(1.0, 2048.0);
            let font_path = cfg
                .get("font")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let h_align = parse_h_align(
                cfg.get("horizontalalign")
                    .and_then(|v| v.as_str())
                    .unwrap_or("center"),
            );
            let v_align = parse_v_align(
                cfg.get("verticalalign")
                    .and_then(|v| v.as_str())
                    .unwrap_or("center"),
            );
            let box_size = [
                if raw_size[0] > 0.0 { raw_size[0] } else { 512.0 },
                if raw_size[1] > 0.0 { raw_size[1] } else { 128.0 },
            ];
            let font = text::load_font(pkg, &font_path);
            let layer = TextLayer {
                script: None,
                authored,
                raw_text_def: serde_json::Value::Null,
                props: HashMap::new(),
                font,
                font_path,
                point_size,
                spacing: [0.0, 0.0],
                padding: [0.0, 0.0],
                max_width: 0.0,
                limit_width: false,
                limit_rows: false,
                max_rows: 1,
                limit_use_ellipsis: false,
                opaque_background: false,
                background_color: [0.0, 0.0, 0.0],
                background_brightness: 1.0,
                h_align,
                v_align,
                box_size,
                world_size: box_size,
                current: String::new(),
                texture: None,
                raster_size: [0, 0],
                raster_ppw: 0.0,
            };
            (
                ObjectKind::Text(Box::new(layer)),
                box_size,
                LayerBlend::Normal,
                None,
            )
        } else if kind_str == "solid"
            || image_ref.to_ascii_lowercase().contains("util/solidlayer")
            || image_ref.is_empty()
        {
            let size = [
                if raw_size[0] > 0.0 { raw_size[0] } else { 100.0 },
                if raw_size[1] > 0.0 { raw_size[1] } else { 100.0 },
            ];
            (ObjectKind::Solid, size, LayerBlend::Normal, None)
        } else {
            let info = resolve_image_model(pkg, &image_ref);
            let loaded = info
                .texture_path
                .as_deref()
                .and_then(|tp| Self::load_texture(gl, pkg, &mut self.textures, tp));
            if let Some((tex, tw, th, ss)) = loaded {
                let (fw, fh) = match ss {
                    Some(s) => (tw / s.cols.max(1), th / s.rows.max(1)),
                    None => (tw, th),
                };
                let def_sz = info.model_size.unwrap_or([fw as f32, fh as f32]);
                let size = [
                    if raw_size[0] > 0.0 { raw_size[0] } else { def_sz[0] },
                    if raw_size[1] > 0.0 { raw_size[1] } else { def_sz[1] },
                ];
                (
                    ObjectKind::Image(tex),
                    size,
                    LayerBlend::from_material(&info.material_blend),
                    ss,
                )
            } else {
                let size = [
                    if raw_size[0] > 0.0 { raw_size[0] } else { 100.0 },
                    if raw_size[1] > 0.0 { raw_size[1] } else { 100.0 },
                ];
                (ObjectKind::Solid, size, LayerBlend::Normal, None)
            }
        };

        self.objects.push(SceneObject {
            id: Some(id),
            name,
            transform: local,
            size,
            align,
            color: [color_rgb[0], color_rgb[1], color_rgb[2], alpha],
            brightness: 1.0,
            blend,
            color_blend: 0,
            parallax,
            visible: VisibleRule::Always,
            solid,
            dependencies: Vec::new(),
            capture_composite: false,
            effects: Vec::new(),
            kind,
            anim: None,
            sprite,
            attach: None,
            raw_idx: None,
            raw_obj: serde_json::Value::Null,
            parent_obj,
            local_parts: (origin, scale, angles),
            script_visible: Some(visible),
            script_text_override: None,
            script_sprite_frame: None,
            user_tex: None,
        });
        let new_idx = self.objects.len() - 1;
        self.script_ids.insert(id, new_idx);
        self.script_objs.push(new_idx);
        true
    }

    fn destroy_dynamic_layer(&mut self, gl: &glow::Context, id: i64) {
        let Some(&idx) = self.script_ids.get(&id) else {
            return;
        };
        let old_len = self.objects.len();
        let mut obj = self.objects.remove(idx);
        for eff in obj.effects.drain(..) {
            eff.destroy(gl);
        }
        if let ObjectKind::Text(layer) = &mut obj.kind {
            if let Some(tex) = layer.texture.take() {
                unsafe { gl.delete_texture(tex) };
            }
        }
        self.cursor_hovered.remove(&id);
        self.cursor_pressed.remove(&id);
        let map = remap_indices_after_remove(old_len, idx);
        self.apply_object_index_remap(&map);
    }

    fn sort_scene_layer(&mut self, id: i64, target_idx: usize) {
        let Some(&from) = self.script_ids.get(&id) else {
            return;
        };
        let len = self.objects.len();
        if len == 0 {
            return;
        }
        let to = target_idx.min(len - 1);
        if from == to {
            return;
        }
        let obj = self.objects.remove(from);
        self.objects.insert(to, obj);
        let map = remap_indices_after_move(len, from, to);
        self.apply_object_index_remap(&map);
    }

    /// Apply SceneScript outputs (layer fields, effect visibility, shader constants)
    /// and propagate any transform updates down the parent chain.
    fn apply_script_outputs(
        &mut self,
        gl: &glow::Context,
        pkg_override: Option<&PackageFs>,
        outputs: Vec<ScriptOutput>,
    ) {
        if outputs.is_empty() {
            return;
        }
        let mut transforms_dirty = false;
        let mut sounds_dirty = false;
        for out in outputs {
            match out.target {
                ScriptTarget::Layer { id, field } => {
                    let Some(&idx) = self.script_ids.get(&id) else {
                        continue;
                    };
                    let pkg = pkg_override.unwrap_or(&self.pkg);
                    let obj = &mut self.objects[idx];
                    match field {
                        ScriptField::Visible => {
                            obj.script_visible = Some(out.value.as_bool().unwrap_or(true));
                            if matches!(obj.kind, ObjectKind::Sound(_)) {
                                sounds_dirty = true;
                            }
                        }
                        ScriptField::Alpha => {
                            let a = out.value.as_f64().unwrap_or(1.0) as f32;
                            obj.color[3] = a.clamp(0.0, 1.0);
                        }
                        ScriptField::Color => {
                            let c = crate::render::scenescript::prop_vec3(&out.value, [1.0; 3]);
                            obj.color[0] = c[0];
                            obj.color[1] = c[1];
                            obj.color[2] = c[2];
                        }
                        ScriptField::Text => {
                            obj.script_text_override = Some(value_to_string(&out.value));
                        }
                        ScriptField::Alignment => {
                            obj.align = parse_alignment_str(&value_to_string(&out.value));
                        }
                        ScriptField::HorizontalAlign => {
                            let ha = parse_h_align(&value_to_string(&out.value));
                            if let ObjectKind::Text(layer) = &mut obj.kind {
                                if layer.h_align != ha {
                                    layer.h_align = ha;
                                    layer.raster_ppw = -1.0;
                                }
                            }
                        }
                        ScriptField::VerticalAlign => {
                            let va = parse_v_align(&value_to_string(&out.value));
                            if let ObjectKind::Text(layer) = &mut obj.kind {
                                if layer.v_align != va {
                                    layer.v_align = va;
                                    layer.raster_ppw = -1.0;
                                }
                            }
                        }
                        ScriptField::PointSize => {
                            if let Some(ps) = out
                                .value
                                .as_f64()
                                .map(|f| f as f32)
                                .filter(|v| v.is_finite() && *v > 0.0)
                                .map(|v| v.clamp(1.0, 2048.0))
                            {
                                if let ObjectKind::Text(layer) = &mut obj.kind {
                                    if (ps - layer.point_size).abs() > 1e-3 {
                                        layer.point_size = ps;
                                        layer.raster_ppw = -1.0;
                                    }
                                }
                            }
                        }
                        ScriptField::Font => {
                            let new_path = value_to_string(&out.value);
                            if !new_path.is_empty() {
                                if let ObjectKind::Text(layer) = &mut obj.kind {
                                    if new_path != layer.font_path {
                                        layer.font_path = new_path;
                                        layer.font = text::load_font(pkg, &layer.font_path);
                                        layer.raster_ppw = -1.0;
                                    }
                                }
                            }
                        }
                        ScriptField::Volume => {
                            let v = (out.value.as_f64().unwrap_or(1.0) as f32).clamp(0.0, 1.0);
                            if let ObjectKind::Sound(sidx) = obj.kind {
                                if let Some(snd) = self.sounds.get_mut(sidx) {
                                    snd.script_volume = Some(v);
                                    sounds_dirty = true;
                                }
                            }
                        }
                        ScriptField::Origin | ScriptField::Scale | ScriptField::Angles => {
                            let v3 = crate::render::scenescript::prop_vec3(&out.value, [0.0; 3]);
                            match field {
                                ScriptField::Origin => obj.local_parts.0 = v3,
                                ScriptField::Scale => obj.local_parts.1 = v3,
                                _ => {
                                    obj.local_parts.2 = [
                                        v3[0].to_radians(),
                                        v3[1].to_radians(),
                                        v3[2].to_radians(),
                                    ];
                                }
                            }
                            transforms_dirty = true;
                        }
                    }
                }
                ScriptTarget::EffectVisible {
                    layer_id,
                    effect_idx,
                } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let Some(eff) = self.objects[idx]
                        .effects
                        .iter_mut()
                        .find(|e| e.scene_idx == effect_idx)
                    {
                        eff.script_visible = Some(out.value.as_bool().unwrap_or(true));
                    }
                }
                ScriptTarget::EffectConstant {
                    layer_id,
                    effect_idx,
                    pass_idx,
                    key,
                } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let Some(eff) = self.objects[idx]
                        .effects
                        .iter_mut()
                        .find(|e| e.scene_idx == effect_idx)
                    {
                        eff.set_script_constant(pass_idx, &key, out.value);
                    }
                }
                ScriptTarget::LayerAnimControl { id, field } => {
                    let Some(&idx) = self.script_ids.get(&id) else {
                        continue;
                    };
                    let obj = &mut self.objects[idx];
                    if let Some(anim) = obj.anim.as_deref_mut() {
                        let (base_origin, base_scale, base_angles, base_alpha) =
                            (anim.origin, anim.scale, anim.angles, anim.base_alpha);
                        if let Some(track) = anim.track_mut(field) {
                            apply_keytrack_ctrl(track, &out.value);
                            match field {
                                ScriptField::Origin => {
                                    obj.local_parts.0 = track.apply3(base_origin);
                                    transforms_dirty = true;
                                }
                                ScriptField::Scale => {
                                    obj.local_parts.1 = track.apply3(base_scale);
                                    transforms_dirty = true;
                                }
                                ScriptField::Angles => {
                                    obj.local_parts.2 = track.apply3(base_angles);
                                    transforms_dirty = true;
                                }
                                ScriptField::Alpha => {
                                    obj.color[3] =
                                        track.apply3([base_alpha; 3])[0].clamp(0.0, 1.0);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                ScriptTarget::EffectConstAnimControl {
                    layer_id,
                    effect_idx,
                    pass_idx,
                    key,
                } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let Some(eff) = self.objects[idx]
                        .effects
                        .iter_mut()
                        .find(|e| e.scene_idx == effect_idx)
                    {
                        if let Some(track) = eff.constant_track_mut(pass_idx, &key) {
                            apply_keytrack_ctrl(track, &out.value);
                        }
                    }
                }
                ScriptTarget::TextureAnimation { layer_id } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let Some(f) = out.value.get("frame").and_then(|v| v.as_f64()) {
                        self.objects[idx].script_sprite_frame = Some(f.max(0.0) as u32);
                    }
                }
                ScriptTarget::VideoTexture { layer_id } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let ObjectKind::Image(tex) = self.objects[idx].kind {
                        if let Some(vt) = self.textures.video_by_texture_mut(tex) {
                            if let Some(playing) =
                                out.value.get("playing").and_then(|v| v.as_bool())
                            {
                                vt.set_script_paused(!playing);
                            }
                            if let Some(rate) = out.value.get("rate").and_then(|v| v.as_f64()) {
                                vt.set_playback_rate(rate as f32);
                            }
                            if let Some(seek) = out.value.get("seek").and_then(|v| v.as_f64()) {
                                vt.set_current_time(seek);
                            }
                        }
                    }
                }
                ScriptTarget::PuppetAnimLayerField {
                    layer_id,
                    anim_layer_idx,
                    field,
                } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let ObjectKind::Puppet {
                        anim_layers,
                        anim_meta,
                        ..
                    } = &mut self.objects[idx].kind
                    {
                        if let Some(pos) = anim_layers
                            .iter()
                            .position(|l| l.scene_idx == anim_layer_idx)
                        {
                            match field {
                                AnimLayerField::Visible => {
                                    let v = out.value.as_bool().unwrap_or(true);
                                    anim_meta[pos].script_visible = Some(v);
                                    anim_layers[pos].visible = v;
                                }
                                AnimLayerField::Blend => {
                                    let b =
                                        (out.value.as_f64().unwrap_or(1.0) as f32).clamp(0.0, 1.0);
                                    anim_meta[pos].script_blend = Some(b);
                                    anim_layers[pos].blend = b;
                                }
                                AnimLayerField::Rate => {
                                    let r = out.value.as_f64().unwrap_or(1.0) as f32;
                                    if r.is_finite() {
                                        anim_meta[pos].script_rate = Some(r);
                                        anim_layers[pos].rate = r;
                                    }
                                }
                            }
                        }
                    }
                }
                ScriptTarget::PuppetAnimLayerControl {
                    layer_id,
                    anim_layer_idx,
                } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let ObjectKind::Puppet {
                        mdl,
                        anim_layers,
                        anim_meta,
                        ..
                    } = &mut self.objects[idx].kind
                    {
                        if let Some(pos) = anim_layers
                            .iter()
                            .position(|l| l.scene_idx == anim_layer_idx)
                        {
                            let al = &mut anim_layers[pos];
                            if let Some(playing) =
                                out.value.get("playing").and_then(|v| v.as_bool())
                            {
                                al.playing = playing;
                            }
                            if let Some(frame) = out.value.get("frame").and_then(|v| v.as_f64()) {
                                let max_idx = mdl
                                    .find_anim(al.anim_id)
                                    .map(|a| mdl.anim_frame_count(a).saturating_sub(1) as f32)
                                    .unwrap_or(0.0);
                                al.frame = (frame as f32).clamp(0.0, max_idx.max(0.0));
                            }
                            if let Some(mr) =
                                out.value.get("mirrorReverse").and_then(|v| v.as_bool())
                            {
                                al.mirror_reverse = mr;
                            }
                            if anim_meta[pos].rate_track.is_none()
                                && anim_meta[pos].script_rate.is_none()
                            {
                                if let Some(rate) = out.value.get("rate").and_then(|v| v.as_f64()) {
                                    let r = rate as f32;
                                    if r.is_finite() {
                                        al.rate = r;
                                    }
                                }
                            }
                        }
                    }
                }
                ScriptTarget::InstanceOverride { layer_id, key } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let ObjectKind::Particle(pidx) = self.objects[idx].kind {
                        self.update_emitter_tree_override(pidx, &key, &out.value);
                    }
                }
                ScriptTarget::LayerSolid { id } => {
                    let Some(&idx) = self.script_ids.get(&id) else {
                        continue;
                    };
                    self.objects[idx].solid = out.value.as_bool().unwrap_or(true);
                }
                ScriptTarget::SoundControl { layer_id } => {
                    let Some(&idx) = self.script_ids.get(&layer_id) else {
                        continue;
                    };
                    if let ObjectKind::Sound(sidx) = self.objects[idx].kind {
                        let vis = self.is_obj_visible(idx);
                        let (master_vol, muted, paused, started) =
                            (self.volume, self.muted, self.paused, self.sounds_started);
                        if let Some(snd) = self.sounds.get_mut(sidx) {
                            match out.value.get("action").and_then(|v| v.as_str()) {
                                Some("play") => {
                                    snd.cmd_play(master_vol, muted, paused, vis, started)
                                }
                                Some("pause") => snd.cmd_pause(),
                                Some("stop") => snd.cmd_stop(),
                                _ => {}
                            }
                            sounds_dirty = true;
                        }
                    }
                }
                ScriptTarget::SceneCreateLayer { id } => {
                    if self.create_dynamic_layer(gl, pkg_override, id, &out.value) {
                        transforms_dirty = true;
                    }
                }
                ScriptTarget::SceneDestroyLayer { id } => {
                    self.destroy_dynamic_layer(gl, id);
                }
                ScriptTarget::SceneSortLayer { id } => {
                    if let Some(target_idx) = out
                        .value
                        .as_u64()
                        .or_else(|| out.value.as_i64().filter(|&v| v >= 0).map(|v| v as u64))
                    {
                        self.sort_scene_layer(id, target_idx as usize);
                    }
                }
            }
        }
        if transforms_dirty {
            self.propagate_transforms();
        }
        if sounds_dirty {
            self.sync_sounds();
        }
    }

    fn collect_cursor_events(
        &mut self,
        cursor_world: [f32; 2],
        cursor_screen: [f32; 2],
        mouse_down: bool,
        time: f32,
    ) {
        self.cursor_events.clear();
        if self.scripts.as_ref().is_none_or(|h| !h.has_cursor_layers()) {
            self.prev_mouse_down = mouse_down;
            self.prev_cursor_world = Some(cursor_world);
            return;
        }
        let just_pressed = mouse_down && !self.prev_mouse_down;
        let just_released = !mouse_down && self.prev_mouse_down;
        let moved = self.prev_cursor_world.is_some_and(|p| {
            (cursor_world[0] - p[0]).abs() > 1e-3 || (cursor_world[1] - p[1]).abs() > 1e-3
        });

        let mut press_claimed = false;
        for i in (0..self.objects.len()).rev() {
            let Some(id) = self.objects[i].id else {
                continue;
            };
            if !self
                .scripts
                .as_ref()
                .is_some_and(|h| h.is_cursor_layer(id))
            {
                continue;
            }
            let is_vis = self.is_obj_visible(i);
            let depth = self.effective_parallax(i);
            let offset = self.parallax_offset(depth, time);
            let obj = &self.objects[i];
            let hit = obj.hit_test(cursor_world, offset);
            let is_hit = hit.is_some();
            let was_hovered = self.cursor_hovered.contains(&id);
            let was_pressed = self.cursor_pressed.contains(&id);
            let local_pos = hit.unwrap_or_else(|| obj.world_to_local(cursor_world, offset));

            if is_hit && !was_hovered {
                self.cursor_hovered.insert(id);
                self.cursor_events.push(CursorEventInput {
                    layer_id: id,
                    kind: CursorEventKind::Enter,
                    world_pos: cursor_world,
                    screen_pos: cursor_screen,
                    local_pos,
                });
            } else if !is_hit && was_hovered {
                self.cursor_hovered.remove(&id);
                self.cursor_events.push(CursorEventInput {
                    layer_id: id,
                    kind: CursorEventKind::Leave,
                    world_pos: cursor_world,
                    screen_pos: cursor_screen,
                    local_pos,
                });
            }
            if just_pressed && claim_cursor_press(is_hit, is_vis, &mut press_claimed) {
                self.cursor_pressed.insert(id);
                self.cursor_events.push(CursorEventInput {
                    layer_id: id,
                    kind: CursorEventKind::Down,
                    world_pos: cursor_world,
                    screen_pos: cursor_screen,
                    local_pos,
                });
            }
            if moved && (is_hit || was_pressed) {
                self.cursor_events.push(CursorEventInput {
                    layer_id: id,
                    kind: CursorEventKind::Move,
                    world_pos: cursor_world,
                    screen_pos: cursor_screen,
                    local_pos,
                });
            }
            if just_released && was_pressed {
                self.cursor_pressed.remove(&id);
                self.cursor_events.push(CursorEventInput {
                    layer_id: id,
                    kind: CursorEventKind::Up,
                    world_pos: cursor_world,
                    screen_pos: cursor_screen,
                    local_pos,
                });
                if is_hit {
                    self.cursor_events.push(CursorEventInput {
                        layer_id: id,
                        kind: CursorEventKind::Click,
                        world_pos: cursor_world,
                        screen_pos: cursor_screen,
                        local_pos,
                    });
                }
            }
        }
        self.prev_mouse_down = mouse_down;
        self.prev_cursor_world = Some(cursor_world);
    }

    fn sync_media_textures(&mut self, gl: &glow::Context, media: &crate::media::MediaSnapshot) {
        let cur_art_rev = media.art.as_ref().map(|a| a.rev);
        let prev_art_rev = media.prev_art.as_ref().map(|a| a.rev);
        if let Some((old_rev, _, _, _)) = self.media_thumb_tex {
            if cur_art_rev != Some(old_rev) && prev_art_rev == Some(old_rev) {
                if let Some((_, stale_prev, _, _)) = self.media_prev_thumb_tex.take() {
                    unsafe { gl.delete_texture(stale_prev) };
                }
                self.media_prev_thumb_tex = self.media_thumb_tex.take();
            }
        }
        sync_media_tex_slot(gl, &mut self.media_thumb_tex, media.art.as_deref());
        sync_media_tex_slot(gl, &mut self.media_prev_thumb_tex, media.prev_art.as_deref());
    }

    fn collect_media_events(&mut self, media: Option<&crate::media::MediaSnapshot>) {
        self.media_events.clear();
        let Some(media) = media else {
            self.media_status_sent = false;
            return;
        };

        let make_props = || MediaEventInput::Properties {
            title: media.title.clone(),
            artist: media.artist.clone(),
            album_title: media.album_title.clone(),
            album_artist: media.album_artist.clone(),
            sub_title: media.sub_title.clone(),
            genres: media.genres.clone(),
            content_type: media.content_type.clone(),
        };
        let make_thumb = || MediaEventInput::Thumbnail {
            has_thumbnail: media.has_thumbnail,
            primary_color: media.palette.primary,
            secondary_color: media.palette.secondary,
            tertiary_color: media.palette.tertiary,
            text_color: media.palette.text,
            high_contrast_color: media.palette.high_contrast,
        };

        if !self.media_status_sent {
            self.media_events.push(MediaEventInput::Status {
                enabled: media.enabled,
            });
            self.media_events.push(MediaEventInput::Playback {
                state: media.state.as_we_code(),
            });
            self.media_events.push(make_props());
            self.media_events.push(make_thumb());
            self.media_events.push(MediaEventInput::Timeline {
                position: media.position,
                duration: media.duration,
            });
            self.media_status_sent = true;
            self.last_media_state = media.state;
            self.last_media_props_rev = media.props_rev;
            self.last_media_thumb_rev = media.thumb_rev;
            self.last_media_pos = media.position;
            self.last_media_dur = media.duration;
            return;
        }

        let state_changed = media.state != self.last_media_state;
        let props_changed = media.props_rev != self.last_media_props_rev;
        let thumb_changed = media.thumb_rev != self.last_media_thumb_rev;

        if state_changed {
            self.media_events.push(MediaEventInput::Playback {
                state: media.state.as_we_code(),
            });
            self.last_media_state = media.state;
        }
        if props_changed {
            self.media_events.push(make_props());
            self.last_media_props_rev = media.props_rev;
        }
        if thumb_changed {
            self.media_events.push(make_thumb());
            self.last_media_thumb_rev = media.thumb_rev;
        }

        let dur_changed = (media.duration - self.last_media_dur).abs() > 0.01;
        let pos_changed = (media.position - self.last_media_pos).abs() >= 0.25;
        if dur_changed
            || state_changed
            || props_changed
            || (pos_changed
                && (media.duration > 0.0 || media.state == crate::media::MediaPlaybackState::Playing))
        {
            self.media_events.push(MediaEventInput::Timeline {
                position: media.position,
                duration: media.duration,
            });
            self.last_media_pos = media.position;
            self.last_media_dur = media.duration;
        }
    }

    /// Run one frame of SceneScripts and apply the produced property values.
    #[allow(clippy::too_many_arguments)]
    fn tick_scripts(
        &mut self,
        gl: &glow::Context,
        time: f32,
        dt: f32,
        daytime: f32,
        now: text::LocalTime,
        cursor_world: [f32; 2],
        cursor_screen: [f32; 2],
        mouse_down: bool,
        ortho: [f32; 2],
        spectrum: &[f32; 64],
    ) {
        if self.scripts.as_ref().is_none_or(|h| h.is_empty()) {
            return;
        }

        self.script_frames.clear();
        for &idx in &self.script_objs {
            let props = &self.properties;
            let obj = &self.objects[idx];
            let Some(id) = obj.id else { continue };
            let visible = obj.is_own_visible(props);
            let alpha = obj.color[3];
            let color = [obj.color[0], obj.color[1], obj.color[2]];
            let text = match &obj.kind {
                ObjectKind::Text(layer) => obj
                    .script_text_override
                    .clone()
                    .unwrap_or_else(|| layer.evaluate(props, now)),
                _ => obj.script_text_override.clone().unwrap_or_default(),
            };
            let video_time = match &obj.kind {
                ObjectKind::Image(tex) => self
                    .textures
                    .video_by_texture(*tex)
                    .map(|vt| (vt.current_time() as f32, vt.duration() as f32)),
                _ => None,
            };
            let sound = match &obj.kind {
                ObjectKind::Sound(sidx) => self
                    .sounds
                    .get(*sidx)
                    .map(|s| (s.effective_layer_volume(), s.is_playing())),
                _ => None,
            };
            self.script_frames.push(LayerFrame {
                id,
                visible,
                alpha,
                origin: obj.local_parts.0,
                scale: obj.local_parts.1,
                angles: [
                    obj.local_parts.2[0].to_degrees(),
                    obj.local_parts.2[1].to_degrees(),
                    obj.local_parts.2[2].to_degrees(),
                ],
                color,
                text,
                video_time,
                sound,
            });
        }

        let screen = [self.width as f32, self.height as f32];
        let outputs = {
            let Some(host) = self.scripts.as_mut() else {
                return;
            };
            host.tick_with_input(
                time,
                dt,
                daytime,
                cursor_world,
                ortho,
                screen,
                Some(spectrum),
                &self.script_frames,
                cursor_screen,
                mouse_down,
                &self.cursor_events,
                &self.media_events,
            )
        };
        self.apply_script_outputs(gl, None, outputs);
    }

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

    fn poll_sounds(&mut self) {
        for sound in &mut self.sounds {
            sound.poll();
        }
    }

    fn sync_sounds(&mut self) {
        let mut vis_by_sound = vec![true; self.sounds.len()];
        for i in 0..self.objects.len() {
            if let ObjectKind::Sound(sidx) = self.objects[i].kind {
                if sidx < vis_by_sound.len() {
                    vis_by_sound[sidx] = self.is_obj_visible(i);
                }
            }
        }
        for (sidx, sound) in self.sounds.iter_mut().enumerate() {
            let vis = vis_by_sound.get(sidx).copied().unwrap_or(true);
            sound.sync_player(self.volume, self.muted, self.paused, vis, self.sounds_started);
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
        let limit_width = obj
            .get("limitwidth")
            .map(|v| resolve_prop(v, props_map))
            .and_then(|v| value_bool(&v))
            .unwrap_or(false);
        let limit_rows = obj
            .get("limitrows")
            .map(|v| resolve_prop(v, props_map))
            .and_then(|v| value_bool(&v))
            .unwrap_or(false);
        let max_rows = obj
            .get("maxrows")
            .map(|v| resolve_prop(v, props_map))
            .and_then(|v| value_f32(&v))
            .map(|f| f.round().max(1.0) as usize)
            .unwrap_or(1);
        let limit_use_ellipsis = obj
            .get("limituseellipsis")
            .map(|v| resolve_prop(v, props_map))
            .and_then(|v| value_bool(&v))
            .unwrap_or(false);
        let opaque_background = obj
            .get("opaquebackground")
            .map(|v| resolve_prop(v, props_map))
            .and_then(|v| value_bool(&v))
            .unwrap_or(false);
        let background_color =
            eval_color_field(obj.get("backgroundcolor"), props_map, [0.0, 0.0, 0.0]);
        let background_brightness =
            eval_brightness_field(obj.get("backgroundbrightness"), props_map, 1.0);
        let h_align = parse_h_align(
            obj.get("horizontalalign")
                .and_then(|v| v.as_str())
                .unwrap_or("center"),
        );
        let v_align = parse_v_align(
            obj.get("verticalalign")
                .and_then(|v| v.as_str())
                .unwrap_or("center"),
        );
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
            font_path,
            point_size,
            spacing: [0.0, 0.0],
            padding,
            max_width,
            limit_width,
            limit_rows,
            max_rows,
            limit_use_ellipsis,
            opaque_background,
            background_color,
            background_brightness,
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
                solid: true,
                dependencies: Vec::new(),
                capture_composite: false,
                effects: Vec::new(),
                kind: ObjectKind::Image(texture),
                anim: None,
                sprite: None,
                attach: None,
                raw_idx: None,
                raw_obj: serde_json::Value::Null,
                parent_obj: None,
                local_parts: (
                    origin,
                    [scale[0], scale[1], 1.0],
                    [0.0, 0.0, l.rotation.unwrap_or(0.0)],
                ),
                script_visible: None,
                script_text_override: None,
                script_sprite_frame: None,
                user_tex: None,
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
                solid: true,
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
                parent_obj: None,
                local_parts: ([0.0; 3], [1.0; 3], [0.0; 3]),
                script_visible: None,
                script_text_override: None,
                script_sprite_frame: None,
                user_tex: None,
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
        self.properties.insert(existing_key.clone(), parsed.clone());
        self.pending_user_props.insert(existing_key, parsed);

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
        if let Some(general) = self.scene_general.clone() {
            self.bloom = parse_bloom_config(&general, &self.properties);
            if let Some(cc) = general.get("clearcolor").filter(|v| v.get("user").is_some()) {
                self.clear_color = eval_color_field(Some(cc), &self.properties, self.clear_color);
            }
            self.apply_general_camera_settings(&general);
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

        let has_binding = |id: Option<i64>, field: ScriptField| -> bool {
            self.scripts
                .as_ref()
                .is_some_and(|s| id.is_some_and(|id| s.has_layer_binding(id, field)))
        };
        let mut transforms_dirty = false;
        for obj in &mut self.objects {
            if obj.raw_obj.is_null() {
                continue;
            }
            let raw = &obj.raw_obj;
            let obj_id = obj.id;
            let alpha = if has_binding(obj_id, ScriptField::Alpha) {
                obj.color[3]
            } else {
                eval_alpha_field(raw.get("alpha"), &self.properties, now)
            };
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
                    let color_scripted = self.scripts.is_some()
                        && raw.get("instanceoverride").is_some_and(|io| {
                            io.get("colorn")
                                .or_else(|| io.get("color"))
                                .and_then(|v| v.get("script"))
                                .and_then(|s| s.as_str())
                                .is_some()
                        });
                    if !color_scripted {
                        if let Some(color_mult) = raw
                            .get("instanceoverride")
                            .and_then(|o| o.get("colorn").or_else(|| o.get("color")))
                            .or_else(|| raw.get("color"))
                            .map(|c| {
                                parse_color(&resolve_prop(c, &self.properties), [1.0, 1.0, 1.0])
                            })
                        {
                            Self::update_emitter_tree_color(&mut self.emitters, *idx, color_mult);
                        }
                    }
                }
                ObjectKind::Sound(sidx) => {
                    let new_base = eval_sound_volume(raw.get("volume"), &self.properties);
                    if let Some(snd) = self.sounds.get_mut(*sidx) {
                        if (new_base - snd.base_volume).abs() > 1e-4 {
                            snd.base_volume = new_base;
                            snd.script_volume = None;
                        }
                    }
                }
                ObjectKind::Text(layer) => {
                    if !has_binding(obj_id, ScriptField::Color) {
                        let c = eval_color_field(raw.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                        obj.color[0] = c[0];
                        obj.color[1] = c[1];
                        obj.color[2] = c[2];
                    }
                    obj.color[3] = alpha;
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
                    if let Some(ob) = raw
                        .get("opaquebackground")
                        .map(|v| resolve_prop(v, &self.properties))
                        .and_then(|v| value_bool(&v))
                    {
                        layer.opaque_background = ob;
                    }
                    if raw.get("backgroundcolor").is_some() {
                        layer.background_color = eval_color_field(
                            raw.get("backgroundcolor"),
                            &self.properties,
                            layer.background_color,
                        );
                    }
                    if raw.get("backgroundbrightness").is_some() {
                        layer.background_brightness = eval_brightness_field(
                            raw.get("backgroundbrightness"),
                            &self.properties,
                            layer.background_brightness,
                        );
                    }
                }
                ObjectKind::Image(_) | ObjectKind::Solid | ObjectKind::CompositeRef(_) => {
                    if !has_binding(obj_id, ScriptField::Color) {
                        let c = eval_color_field(raw.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                        obj.color[0] = c[0];
                        obj.color[1] = c[1];
                        obj.color[2] = c[2];
                    }
                    obj.color[3] = alpha;
                    obj.brightness =
                        eval_brightness_field(raw.get("brightness"), &self.properties, 1.0);
                    obj.parallax = eval_parallax_field(raw.get("parallaxDepth"), &self.properties);
                }
                ObjectKind::Puppet {
                    anim_layers,
                    anim_meta,
                    ..
                } => {
                    if !has_binding(obj_id, ScriptField::Color) {
                        let c = eval_color_field(raw.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                        obj.color[0] = c[0];
                        obj.color[1] = c[1];
                        obj.color[2] = c[2];
                    }
                    obj.color[3] = alpha;
                    obj.brightness =
                        eval_brightness_field(raw.get("brightness"), &self.properties, 1.0);
                    obj.parallax = eval_parallax_field(raw.get("parallaxDepth"), &self.properties);
                    if let Some(raw_als) = raw.get("animationlayers").and_then(|v| v.as_array()) {
                        for (al, meta) in anim_layers.iter_mut().zip(anim_meta.iter()) {
                            let Some(raw_al) = raw_als.get(al.scene_idx) else {
                                continue;
                            };
                            if meta.script_visible.is_none() {
                                al.visible =
                                    parse_visible(meta.visible_raw.as_ref(), &self.properties, now)
                                        .eval(&self.properties);
                            }
                            if meta.script_blend.is_none() && meta.blend_track.is_none() {
                                al.blend = raw_al
                                    .get("blend")
                                    .map(|v| resolve_prop(v, &self.properties))
                                    .as_ref()
                                    .and_then(value_f32)
                                    .unwrap_or(1.0)
                                    .clamp(0.0, 1.0);
                            }
                            if meta.script_rate.is_none() && meta.rate_track.is_none() {
                                al.rate = raw_al
                                    .get("rate")
                                    .map(|v| resolve_prop(v, &self.properties))
                                    .as_ref()
                                    .and_then(value_f32)
                                    .unwrap_or(1.0);
                            }
                        }
                    }
                }
            }

            if let Some(ridx) = obj.raw_idx {
                if let Some(&(_, _, origin, scale, angles)) = transforms.get(ridx) {
                    let s_origin = has_binding(obj_id, ScriptField::Origin);
                    let s_scale = has_binding(obj_id, ScriptField::Scale);
                    let s_angles = has_binding(obj_id, ScriptField::Angles);
                    if !s_origin {
                        obj.local_parts.0 = origin;
                    }
                    if !s_scale {
                        let has_scale_dyn = raw.get("scale").is_some_and(|v| {
                            v.get("script").and_then(|s| s.as_str()).is_some()
                                || v.get("animation").is_some()
                        });
                        obj.local_parts.1 = if has_scale_dyn {
                            scale
                        } else {
                            [
                                if scale[0] == 0.0 { 1.0 } else { scale[0] },
                                if scale[1] == 0.0 { 1.0 } else { scale[1] },
                                scale[2],
                            ]
                        };
                    }
                    if !s_angles {
                        obj.local_parts.2 = angles;
                    }
                    if !s_origin || !s_scale || !s_angles {
                        transforms_dirty = true;
                    }
                    if let Some(att) = &mut obj.attach {
                        att.local = Affine::translate(origin[0], origin[1])
                            .mul(Affine::rotate(angles[2]))
                            .mul(Affine::scale(
                                if scale[0] == 0.0 { 1.0 } else { scale[0] },
                                if scale[1] == 0.0 { 1.0 } else { scale[1] },
                            ));
                    } else if !matches!(
                        obj.kind,
                        ObjectKind::Post | ObjectKind::Particle(_) | ObjectKind::Sound(_)
                    ) {
                        let prev_anim = obj.anim.take();
                        let mut next_anim = ObjAnim::parse(
                            raw,
                            origin,
                            scale,
                            angles,
                            &self.properties,
                            now,
                        );
                        if let (Some(n), Some(p)) = (&mut next_anim, &prev_anim) {
                            n.preserve_playback_from(p);
                        }
                        obj.anim = next_anim;
                    }
                }
            }
        }
        if transforms_dirty {
            self.propagate_transforms();
        }
        self.sync_sounds();
        if self.scripts.is_none()
            && self
                .objects
                .iter()
                .any(|o| !o.raw_obj.is_null() && object_has_script(&o.raw_obj))
        {
            self.scripts_dirty = true;
        }
    }

    fn cover_uv_transform(&self) -> ([f32; 2], [f32; 2]) {
        cover_uv_with_zoom(self.ortho_size, [self.width, self.height], self.camera_zoom)
    }

    fn ensure_scene(&mut self, gl: &glow::Context) -> Result<()> {
        let (uv_scale, _) = self.cover_uv_transform();
        let w = ((self.width.max(1) as f32 / uv_scale[0]).round() as i32).clamp(1, 4096);
        let h = ((self.height.max(1) as f32 / uv_scale[1]).round() as i32).clamp(1, 4096);
        let want_hdr = self.bloom.hdr && self.hdr_supported;
        let recreate = match &self.scene {
            Some(c) => c.w != w || c.h != h || c.hdr != want_hdr,
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
            if let Some(old) = self.bloom_a.take() {
                old.destroy(gl);
            }
            if let Some(old) = self.bloom_b.take() {
                old.destroy(gl);
            }
            for old in self.bloom_mips.drain(..) {
                old.destroy(gl);
            }
            for (_, old) in self.composites.drain() {
                old.destroy(gl);
            }
            let scene = if want_hdr {
                Canvas::new_hdr(gl, w, h)?
            } else {
                Canvas::new(gl, w, h)?
            };
            if want_hdr && !scene.hdr {
                self.hdr_supported = false;
            }
            let scene_alt = if scene.hdr {
                Canvas::new_hdr(gl, w, h)?
            } else {
                Canvas::new(gl, w, h)?
            };
            self.scene = Some(scene);
            self.scene_alt = Some(scene_alt);
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
            p.set_i32(gl, "u_has_bloom", 0);
            p.set_i32(gl, "u_tex", 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(src_tex));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_bloom_pass(
        &mut self,
        gl: &glow::Context,
        src_tex: glow::Texture,
        dst_fbo: glow::Framebuffer,
        dst_w: i32,
        dst_h: i32,
        mode: i32,
        texel_step: [f32; 2],
        threshold: f32,
        strength: f32,
        tint: [f32; 3],
        blend_params: [f32; 4],
    ) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst_fbo));
            gl.viewport(0, 0, dst_w, dst_h);
            let p = &mut self.bloom_prog;
            p.bind(gl);
            p.set_i32(gl, "u_mode", mode);
            p.set_vec2(gl, "u_texel_step", texel_step[0], texel_step[1]);
            p.set_f32(gl, "u_threshold", threshold);
            p.set_f32(gl, "u_strength", strength);
            p.set_vec3(gl, "u_tint", tint[0], tint[1], tint[2]);
            p.set_vec4(gl, "u_blend_params", blend_params);
            p.set_i32(gl, "u_tex", 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(src_tex));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    fn render_bloom(&mut self, gl: &glow::Context) -> Result<Option<glow::Texture>> {
        if !self.bloom.enabled {
            return Ok(None);
        }
        let eff_strength = if self.bloom.hdr {
            self.bloom.hdr_strength
        } else {
            self.bloom.strength
        };
        if eff_strength <= 1e-4 {
            return Ok(None);
        }
        let (sw, sh, scene_tex, want_hdr) = {
            let s = self.scene.as_ref().unwrap();
            (s.w, s.h, s.tex, s.hdr)
        };
        unsafe {
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::SCISSOR_TEST);
        }

        if !self.bloom.hdr {
            let bw = (sw / 4).max(1);
            let bh = (sh / 4).max(1);
            if self.bloom_a.as_ref().is_none_or(|c| c.w != bw || c.h != bh) {
                if let Some(old) = self.bloom_a.take() {
                    old.destroy(gl);
                }
                self.bloom_a = Some(Canvas::new(gl, bw, bh)?);
            }
            if self.bloom_b.as_ref().is_none_or(|c| c.w != bw || c.h != bh) {
                if let Some(old) = self.bloom_b.take() {
                    old.destroy(gl);
                }
                self.bloom_b = Some(Canvas::new(gl, bw, bh)?);
            }
            let (a_tex, a_fbo) = {
                let c = self.bloom_a.as_ref().unwrap();
                (c.tex, c.fbo)
            };
            let (b_tex, b_fbo) = {
                let c = self.bloom_b.as_ref().unwrap();
                (c.tex, c.fbo)
            };
            let tint = self.bloom.tint;
            let threshold = self.bloom.threshold;
            let strength = self.bloom.strength;
            self.draw_bloom_pass(
                gl,
                scene_tex,
                a_fbo,
                bw,
                bh,
                0,
                [1.0 / sw.max(1) as f32, 1.0 / sh.max(1) as f32],
                threshold,
                strength,
                tint,
                [0.0; 4],
            );
            self.draw_bloom_pass(
                gl,
                a_tex,
                b_fbo,
                bw,
                bh,
                1,
                [2.0 / bw as f32, 0.0],
                threshold,
                strength,
                tint,
                [0.0; 4],
            );
            self.draw_bloom_pass(
                gl,
                b_tex,
                a_fbo,
                bw,
                bh,
                1,
                [0.0, 2.0 / bh as f32],
                threshold,
                strength,
                tint,
                [0.0; 4],
            );
            return Ok(Some(a_tex));
        }

        let max_iters = self.bloom.hdr_iterations.clamp(1, 8);
        let mut dims = Vec::with_capacity(max_iters);
        let mut mw = (sw / 2).max(1);
        let mut mh = (sh / 2).max(1);
        for _ in 0..max_iters {
            dims.push((mw, mh));
            if mw <= 2 && mh <= 2 {
                break;
            }
            mw = (mw / 2).max(1);
            mh = (mh / 2).max(1);
        }
        let matches = self.bloom_mips.len() == dims.len()
            && self
                .bloom_mips
                .iter()
                .zip(&dims)
                .all(|(c, &(dw, dh))| c.w == dw && c.h == dh && c.hdr == want_hdr);
        if !matches {
            for old in self.bloom_mips.drain(..) {
                old.destroy(gl);
            }
            for &(dw, dh) in &dims {
                let c = if want_hdr {
                    Canvas::new_hdr(gl, dw, dh)?
                } else {
                    Canvas::new(gl, dw, dh)?
                };
                self.bloom_mips.push(c);
            }
        }
        let mips: Vec<(glow::Texture, glow::Framebuffer, i32, i32)> = self
            .bloom_mips
            .iter()
            .map(|c| (c.tex, c.fbo, c.w, c.h))
            .collect();
        let levels = mips.len();
        if levels == 0 {
            return Ok(None);
        }

        let threshold = self.bloom.hdr_threshold.max(0.0);
        let knee = (threshold * self.bloom.hdr_feather.max(0.0)).max(1e-5);
        let blend_params = [threshold, threshold - knee, 2.0 * knee, 0.25 / knee];
        let strength = self.bloom.hdr_strength.max(0.0);
        let tint = self.bloom.tint;

        self.draw_bloom_pass(
            gl,
            scene_tex,
            mips[0].1,
            mips[0].2,
            mips[0].3,
            2,
            [0.5 / sw.max(1) as f32, 0.5 / sh.max(1) as f32],
            threshold,
            strength,
            tint,
            blend_params,
        );
        for k in 1..levels {
            self.draw_bloom_pass(
                gl,
                mips[k - 1].0,
                mips[k].1,
                mips[k].2,
                mips[k].3,
                3,
                [0.5 / mips[k - 1].2.max(1) as f32, 0.5 / mips[k - 1].3.max(1) as f32],
                threshold,
                strength,
                tint,
                blend_params,
            );
        }
        if levels > 1 {
            let scatter = self.bloom.hdr_scatter.max(0.0);
            let scatter_t = (scatter / (1.0 + scatter)).clamp(0.0, 0.92);
            unsafe {
                gl.enable(glow::BLEND);
                gl.blend_color(0.0, 0.0, 0.0, scatter_t);
                gl.blend_func(glow::CONSTANT_ALPHA, glow::ONE_MINUS_CONSTANT_ALPHA);
            }
            for k in (1..levels).rev() {
                self.draw_bloom_pass(
                    gl,
                    mips[k].0,
                    mips[k - 1].1,
                    mips[k - 1].2,
                    mips[k - 1].3,
                    3,
                    [0.5 / mips[k].2.max(1) as f32, 0.5 / mips[k].3.max(1) as f32],
                    threshold,
                    strength,
                    tint,
                    blend_params,
                );
            }
            unsafe {
                gl.disable(glow::BLEND);
            }
        }
        Ok(Some(mips[0].0))
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
            let r = self.camera_shake_roughness.clamp(0.0, 1.0);
            let norm = 1.0 / (1.0 + 0.7 * r);
            let sx = ((t * 1.7).sin() + r * (0.45 * (t * 4.3).sin() + 0.25 * (t * 7.9).cos())) * norm;
            let sy = ((t * 2.3).cos() + r * (0.45 * (t * 5.1).cos() + 0.25 * (t * 8.7).sin())) * norm;
            offset[0] += sx * self.camera_shake_amp * self.ortho_size[0] * 0.01;
            offset[1] += sy * self.camera_shake_amp * self.ortho_size[1] * 0.01;
        }
        offset
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_textured_quad(
        &mut self,
        gl: &glow::Context,
        transform: Affine,
        size: [f32; 2],
        canvas_min: [f32; 2],
        canvas_size: [f32; 2],
        color: [f32; 4],
        brightness: f32,
        uv_rect: [f32; 4],
        premul: bool,
        texture: glow::Texture,
    ) {
        let axis_x = transform.apply_vec([size[0] * 0.5, 0.0]);
        let axis_y = transform.apply_vec([0.0, size[1] * 0.5]);
        let p = &mut self.content_prog;
        unsafe {
            p.bind(gl);
            p.set_vec2(gl, "u_min", canvas_min[0], canvas_min[1]);
            p.set_vec2(gl, "u_canvas", canvas_size[0], canvas_size[1]);
            p.set_vec2(gl, "u_origin", transform.tx, transform.ty);
            p.set_vec2(gl, "u_axis_x", axis_x[0], axis_x[1]);
            p.set_vec2(gl, "u_axis_y", axis_y[0], axis_y[1]);
            p.set_vec4(gl, "u_color", color);
            p.set_f32(gl, "u_brightness", brightness);
            p.set_vec4(gl, "u_uv_rect", uv_rect);
            p.set_i32(gl, "u_premul", i32::from(premul));
            p.set_i32(gl, "u_tex", 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
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
                DrawKind::Image(texture) => {
                    self.draw_textured_quad(
                        gl,
                        transform,
                        info.size,
                        canvas_min,
                        canvas_size,
                        info.color,
                        info.brightness,
                        info.uv_rect,
                        info.premul,
                        texture,
                    );
                }
                DrawKind::Text { texture, bg } => {
                    if let Some((bg_rgb, bg_br)) = bg {
                        if let Some((white, _, _)) = self.textures.get(gl, &self.pkg, "util/white") {
                            self.draw_textured_quad(
                                gl,
                                transform,
                                info.size,
                                canvas_min,
                                canvas_size,
                                [bg_rgb[0], bg_rgb[1], bg_rgb[2], info.color[3]],
                                bg_br,
                                [0.0, 0.0, 1.0, 1.0],
                                info.premul,
                                white,
                            );
                        }
                    }
                    if let Some(texture) = texture {
                        let blend_was_on = bg.is_some() && gl.is_enabled(glow::BLEND);
                        if bg.is_some() && !blend_was_on {
                            gl.enable(glow::BLEND);
                            gl.blend_func_separate(
                                glow::SRC_ALPHA,
                                glow::ONE_MINUS_SRC_ALPHA,
                                glow::ONE,
                                glow::ONE_MINUS_SRC_ALPHA,
                            );
                        }
                        self.draw_textured_quad(
                            gl,
                            transform,
                            info.size,
                            canvas_min,
                            canvas_size,
                            info.color,
                            info.brightness,
                            info.uv_rect,
                            info.premul,
                            texture,
                        );
                        if bg.is_some() && !blend_was_on {
                            gl.disable(glow::BLEND);
                        }
                    }
                }
                DrawKind::None => {}
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
                    self.draw_textured_quad(
                        gl,
                        transform,
                        info.size,
                        canvas_min,
                        canvas_size,
                        info.color,
                        info.brightness,
                        [0.0, 0.0, 1.0, 1.0],
                        info.premul,
                        white,
                    );
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
        let (scene_w, scene_h, scene_fbo, scene_hdr) = {
            let scene = self.scene.as_ref().unwrap();
            (scene.w, scene.h, scene.fbo, scene.hdr)
        };
        let recreate = match &self.scene_copy {
            Some(c) => c.w != scene_w || c.h != scene_h || c.hdr != scene_hdr,
            None => true,
        };
        if recreate {
            if let Some(old) = self.scene_copy.take() {
                old.destroy(gl);
            }
            self.scene_copy = Some(if scene_hdr {
                Canvas::new_hdr(gl, scene_w, scene_h)?
            } else {
                Canvas::new(gl, scene_w, scene_h)?
            });
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
                media_thumbnail: self.media_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                media_prev_thumbnail: self.media_prev_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
            };
            for effect in effects.iter_mut() {
                if !effect_is_visible(effect, ctx.props, now) {
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
        let out = text::rasterize_with_options(
            font,
            &value,
            font_px,
            text::RasterizeOptions {
                padding_px: [self.padding[0] * ppw, self.padding[1] * ppw],
                spacing: spacing_px,
                h_align: self.h_align,
                v_align: self.v_align,
                max_width_px,
                max_rows: if self.limit_rows {
                    self.max_rows.max(1)
                } else {
                    0
                },
                use_ellipsis: self.limit_use_ellipsis,
                fixed_size: fixed,
            },
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

fn apply_keytrack_ctrl(track: &mut KeyTrack, val: &serde_json::Value) {
    let playing = val
        .get("playing")
        .and_then(|v| v.as_bool())
        .unwrap_or(track.playing);
    let frame = val
        .get("frame")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32)
        .unwrap_or(track.frame);
    let rate = val
        .get("rate")
        .and_then(|v| v.as_f64())
        .map(|r| r as f32)
        .unwrap_or(track.rate);
    let mirror_reverse = val
        .get("mirrorReverse")
        .and_then(|v| v.as_bool())
        .unwrap_or(track.mirror_reverse);
    track.apply_script_state(playing, frame, rate, mirror_reverse);
}

/// Normalise a Wallpaper Engine `parent` field to an object id.
fn parent_id(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Invoke `f` for every SceneScript source string attached to `raw`
/// (layer fields, effect visibility, and effect pass constants).
fn eval_sound_volume(
    def: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
) -> f32 {
    let Some(def) = def else {
        return 1.0;
    };
    let resolved = if def.get("user").is_some() {
        resolve_prop(def, props)
    } else if let Some(val) = def.get("value") {
        resolve_prop(val, props)
    } else {
        resolve_prop(def, props)
    };
    value_f32(&resolved).unwrap_or(1.0).clamp(0.0, 1.0)
}

fn visit_object_scripts(raw: &serde_json::Value, mut f: impl FnMut(&str)) {
    const KEYS: [&str; 8] = [
        "visible", "alpha", "origin", "scale", "angles", "color", "text", "volume",
    ];
    for key in KEYS {
        if let Some(script) = raw
            .get(key)
            .and_then(|v| v.get("script"))
            .and_then(|s| s.as_str())
        {
            f(script);
        }
    }
    if let Some(effects) = raw.get("effects").and_then(|e| e.as_array()) {
        for ef in effects {
            if let Some(script) = ef
                .get("visible")
                .and_then(|v| v.get("script"))
                .and_then(|s| s.as_str())
            {
                f(script);
            }
            if let Some(passes) = ef.get("passes").and_then(|p| p.as_array()) {
                for pass in passes {
                    if let Some(csv) = pass
                        .get("constantshadervalues")
                        .and_then(|c| c.as_object())
                    {
                        for cv in csv.values() {
                            if let Some(script) = cv.get("script").and_then(|s| s.as_str()) {
                                f(script);
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(als) = raw.get("animationlayers").and_then(|a| a.as_array()) {
        for al in als {
            for key in ["visible", "blend", "rate"] {
                if let Some(script) = al
                    .get(key)
                    .and_then(|v| v.get("script"))
                    .and_then(|s| s.as_str())
                {
                    f(script);
                }
            }
        }
    }
    if let Some(inst) = raw.get("instanceoverride").and_then(|o| o.as_object()) {
        for ov in inst.values() {
            if let Some(script) = ov.get("script").and_then(|s| s.as_str()) {
                f(script);
            }
        }
    }
}

/// Layer names a SceneScript reaches for through `thisScene.getLayer('name')`.
///
/// The scene table handed to QuickJS covers every layer, but per-frame values
/// are only fed for layers a script can actually observe, so the named
/// lookups have to be discovered from the script source.
fn layer_script_refs(raw: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    visit_object_scripts(raw, |script| collect_get_layer_names(script, &mut out));
    out.sort();
    out.dedup();
    out
}

/// Extract the string literal argument of every `getLayer(...)` call.
fn collect_get_layer_names(src: &str, out: &mut Vec<String>) {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while let Some(pos) = src[i..].find("getLayer") {
        let mut p = i + pos + "getLayer".len();
        i = p;
        while matches!(bytes.get(p), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            p += 1;
        }
        if bytes.get(p) != Some(&b'(') {
            continue;
        }
        p += 1;
        while matches!(bytes.get(p), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            p += 1;
        }
        let Some(&quote @ (b'\'' | b'"')) = bytes.get(p) else {
            continue;
        };
        p += 1;
        let start = p;
        while let Some(&c) = bytes.get(p) {
            if c == quote || c == b'\n' {
                break;
            }
            p += 1;
        }
        if bytes.get(p) == Some(&quote) && p > start {
            if let Ok(name) = std::str::from_utf8(&bytes[start..p]) {
                out.push(name.to_string());
            }
        }
    }
}

/// Objects that carry at least one SceneScript on layer or effect fields.
fn object_has_script(raw: &serde_json::Value) -> bool {
    let mut found = false;
    visit_object_scripts(raw, |_| found = true);
    found
}

/// Whether any script on `raw` calls `enumerateLayers`.
fn object_enumerates_layers(raw: &serde_json::Value) -> bool {
    let mut found = false;
    visit_object_scripts(raw, |s| {
        if s.contains("enumerateLayers") {
            found = true;
        }
    });
    found
}

/// Resolve any `{ "user": ... }` entries inside a `scriptproperties` map.
fn resolve_script_props(
    block: Option<&serde_json::Map<String, serde_json::Value>>,
    props: &HashMap<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    let Some(block) = block else { return out };
    for (name, value) in block {
        let resolved = if value.get("user").is_some() {
            resolve_prop(value, props)
        } else {
            value.clone()
        };
        out.insert(name.clone(), resolved);
    }
    out
}

fn raw_bool_field(
    v: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
) -> bool {
    let Some(v) = v else { return true };
    if v.get("user").is_some() {
        value_bool(&resolve_prop(v, props)).unwrap_or(true)
    } else if let Some(b) = v.as_bool() {
        b
    } else {
        v.get("value").and_then(value_bool).unwrap_or(true)
    }
}

fn effect_is_visible(
    effect: &EffectInstance,
    props: &HashMap<String, serde_json::Value>,
    now: text::LocalTime,
) -> bool {
    effect
        .script_visible
        .unwrap_or_else(|| parse_visible(effect.visible_raw.as_ref(), props, now).eval(props))
}

/// Resolve a scripted property definition into the value handed to
/// `update(value)` / `init(value)` and the property block that overrides the
/// script's declared `createScriptProperties()` defaults.
fn field_script_defaults(
    field: crate::render::scenescript::ScriptField,
    def: &serde_json::Value,
    props: &HashMap<String, serde_json::Value>,
) -> (serde_json::Value, serde_json::Map<String, serde_json::Value>) {
    use crate::render::scenescript::{prop_vec3, ScriptField};

    let script_props = resolve_script_props(
        def.get("scriptproperties").and_then(|p| p.as_object()),
        props,
    );

    let resolved = if def.get("user").is_some() {
        Some(resolve_prop(def, props))
    } else {
        def.get("value").map(|v| resolve_prop(v, props))
    };
    let rv = resolved.as_ref().unwrap_or(&serde_json::Value::Null);
    let initial = match field {
        ScriptField::Visible => {
            serde_json::Value::Bool(resolved.as_ref().and_then(value_bool).unwrap_or(true))
        }
        ScriptField::Alpha | ScriptField::Volume => {
            serde_json::Value::from(resolved.as_ref().and_then(value_f32).unwrap_or(1.0) as f64)
        }
        ScriptField::PointSize => {
            serde_json::Value::from(resolved.as_ref().and_then(value_f32).unwrap_or(32.0) as f64)
        }
        ScriptField::Text
        | ScriptField::Alignment
        | ScriptField::HorizontalAlign
        | ScriptField::VerticalAlign
        | ScriptField::Font => {
            serde_json::Value::String(resolved.map(|v| value_to_string(&v)).unwrap_or_default())
        }
        ScriptField::Color => serde_json::Value::from(prop_vec3(rv, [1.0, 1.0, 1.0])),
        ScriptField::Angles => {
            let mut a = prop_vec3(rv, [0.0, 0.0, 0.0]);
            for c in a.iter_mut() {
                *c = c.to_degrees();
            }
            serde_json::Value::from(a)
        }
        ScriptField::Origin | ScriptField::Scale => {
            let default = if field == ScriptField::Scale {
                [1.0, 1.0, 1.0]
            } else {
                [0.0, 0.0, 0.0]
            };
            serde_json::Value::from(prop_vec3(rv, default))
        }
    };
    (initial, script_props)
}

fn parse_h_align(s: &str) -> text::HAlign {
    match s.trim().to_ascii_lowercase().as_str() {
        "left" => text::HAlign::Left,
        "right" => text::HAlign::Right,
        _ => text::HAlign::Center,
    }
}

fn parse_v_align(s: &str) -> text::VAlign {
    match s.trim().to_ascii_lowercase().as_str() {
        "top" => text::VAlign::Top,
        "bottom" => text::VAlign::Bottom,
        _ => text::VAlign::Center,
    }
}

fn parse_alignment_str(s: &str) -> [f32; 2] {
    let lower = s.trim().to_ascii_lowercase();
    let x = if lower.contains("left") {
        0.5
    } else if lower.contains("right") {
        -0.5
    } else {
        0.0
    };
    let y = if lower.contains("bottom") {
        0.5
    } else if lower.contains("top") {
        -0.5
    } else {
        0.0
    };
    [x, y]
}

fn parse_alignment(obj: &serde_json::Value) -> [f32; 2] {
    parse_alignment_str(obj.get("alignment").and_then(|v| v.as_str()).unwrap_or(""))
}

fn parallax_smoothing_factor(dt: f32, delay: f32) -> f32 {
    (1.0 - (-dt.max(0.0) / delay.max(0.01)).exp()).clamp(0.0, 1.0)
}

fn cover_uv_with_zoom(
    ortho_size: [f32; 2],
    win_size: [i32; 2],
    zoom: f32,
) -> ([f32; 2], [f32; 2]) {
    let ortho_w = ortho_size[0].max(1.0);
    let ortho_h = ortho_size[1].max(1.0);
    let scene_aspect = ortho_w / ortho_h;
    let win_aspect = (win_size[0].max(1) as f32) / (win_size[1].max(1) as f32);
    let (base_sx, base_sy) = if (win_aspect - scene_aspect).abs() < 1e-3 {
        (1.0, 1.0)
    } else if win_aspect > scene_aspect {
        (1.0, (scene_aspect / win_aspect).clamp(0.05, 1.0))
    } else {
        ((win_aspect / scene_aspect).clamp(0.05, 1.0), 1.0)
    };
    let z = zoom.clamp(0.1, 10.0);
    let sx = base_sx / z;
    let sy = base_sy / z;
    ([sx, sy], [(1.0 - sx) * 0.5, (1.0 - sy) * 0.5])
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

pub(super) fn read_scene_text(pkg: &PackageFs, path: &str) -> Option<String> {
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
    for (effect_idx, effect) in effects.iter().enumerate() {
        // Keep effects whose visibility is script- or user-property-driven so toggling
        // properties live in the inspector enables/disables them without reloading.
        if effect.get("visible").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let Some(file) = effect.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(mut instance) = EffectInstance::load(gl, pkg, file, effect) {
            instance.scene_idx = effect_idx;
            out.push(instance);
        }
    }
    out
}

impl Renderer for SceneRenderer {
    fn render(&mut self, gl: &glow::Context, info: &FrameInfo) -> Result<bool> {
        self.ensure_scene(gl)?;
        if self.scripts_dirty {
            self.load_scene_scripts(gl, None);
        } else if !self.pending_user_props.is_empty() {
            let changed = std::mem::take(&mut self.pending_user_props);
            if self.scripts.is_some() {
                let binding_props: Vec<(usize, serde_json::Map<String, serde_json::Value>)> = self
                    .script_binding_props
                    .iter()
                    .map(|(idx, raw_sp)| (*idx, resolve_script_props(Some(raw_sp), &self.properties)))
                    .collect();
                let outputs = self
                    .scripts
                    .as_mut()
                    .map(|host| host.apply_user_properties(&changed, &binding_props))
                    .unwrap_or_default();
                self.apply_script_outputs(gl, None, outputs);
                self.capture_and_flush_storage(false);
            }
        }
        self.sounds_started = true;
        self.poll_sounds();
        // Video-texture layers play their own stream; hand them the frame
        // before anything samples the texture.
        self.textures.update_videos(gl, self.paused);
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
        let lerp = parallax_smoothing_factor(dt, self.parallax_delay);
        self.smooth_mouse[0] += (target_mx - self.smooth_mouse[0]) * lerp;
        self.smooth_mouse[1] += (target_my - self.smooth_mouse[1]) * lerp;
        let (cover_scale, cover_offset) = self.cover_uv_transform();
        let raw_win_uv = [
            ((target_mx + 1.0) * 0.5).clamp(0.0, 1.0),
            ((1.0 - target_my) * 0.5).clamp(0.0, 1.0),
        ];
        let raw_mouse_uv = [
            (cover_offset[0] + raw_win_uv[0] * cover_scale[0]).clamp(0.0, 1.0),
            (cover_offset[1] + raw_win_uv[1] * cover_scale[1]).clamp(0.0, 1.0),
        ];
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
        let cursor_world = [
            raw_mouse_uv[0] * ortho_w,
            (1.0 - raw_mouse_uv[1]) * ortho_h,
        ];
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

        // Evaluate keyframe / property animation and hand every layer to the
        // SceneScript host.  Script results are applied before text and puppet
        // refresh so a script can drive `text`, `visible`, `alpha` and `local_parts`.
        {
            let mut transforms_dirty = false;
            for i in 0..self.objects.len() {
                for eff in &mut self.objects[i].effects {
                    eff.advance_animations(dt);
                }
                if let ObjectKind::Puppet {
                    mdl,
                    anim_layers,
                    anim_meta,
                    ..
                } = &mut self.objects[i].kind
                {
                    let props = &self.properties;
                    for (al, meta) in anim_layers.iter_mut().zip(anim_meta.iter_mut()) {
                        al.visible = meta.script_visible.unwrap_or_else(|| {
                            parse_visible(meta.visible_raw.as_ref(), props, now).eval(props)
                        });
                        if let Some(sb) = meta.script_blend {
                            al.blend = sb;
                        } else if let Some(track) = &mut meta.blend_track {
                            track.advance(dt);
                            al.blend = track.eval()[0].clamp(0.0, 1.0);
                        }
                        if let Some(sr) = meta.script_rate {
                            al.rate = sr;
                        } else if let Some(track) = &mut meta.rate_track {
                            track.advance(dt);
                            al.rate = track.eval()[0];
                        }
                    }
                    if mdl.has_animations() {
                        mdl.advance_layers(dt, anim_layers);
                    }
                }
                if !self.is_obj_visible(i) && !self.objects[i].capture_composite {
                    if let Some(anim) = &mut self.objects[i].anim {
                        anim.advance_tracks(dt);
                    }
                    continue;
                }
                let obj_id = self.objects[i].id;
                let has_script = |field: ScriptField| -> bool {
                    self.scripts
                        .as_ref()
                        .is_some_and(|s| obj_id.is_some_and(|id| s.has_layer_binding(id, field)))
                };
                let skip_origin = has_script(ScriptField::Origin);
                let skip_scale = has_script(ScriptField::Scale);
                let skip_angles = has_script(ScriptField::Angles);
                let obj = &mut self.objects[i];
                if let Some(anim) = &mut obj.anim {
                    let mut parts = obj.local_parts;
                    let (changed, alpha) = anim.apply_to_parts(
                        &mut parts,
                        dt,
                        time,
                        daytime,
                        now,
                        skip_origin,
                        skip_scale,
                        skip_angles,
                    );
                    if changed {
                        obj.local_parts = parts;
                        transforms_dirty = true;
                    }
                    if let Some(a) = alpha {
                        obj.color[3] = a;
                    }
                }
            }
            if transforms_dirty {
                self.propagate_transforms();
            }
        }

        if let Some(media) = info.media.as_ref() {
            self.sync_media_textures(gl, media);
        }
        self.collect_media_events(info.media.as_ref());
        self.collect_cursor_events(cursor_world, raw_win_uv, info.mouse_down, time);
        self.tick_scripts(
            gl,
            time,
            dt,
            daytime,
            now,
            cursor_world,
            raw_win_uv,
            info.mouse_down,
            ortho,
            &info.spectrum,
        );
        self.sync_sounds();
        self.capture_and_flush_storage(false);

        // Refresh text and skeletal puppet meshes before computing layer bounds.
        for i in 0..self.objects.len() {
            if !self.is_obj_visible(i) && !self.objects[i].capture_composite {
                continue;
            }
            let props = &self.properties;
            let obj = &mut self.objects[i];
            match &mut obj.kind {
                ObjectKind::Text(layer) => {
                    let value = match obj.script_text_override.as_ref() {
                        Some(t) => t.clone(),
                        None => layer.evaluate(props, now),
                    };
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
            let has_local_script = self.scripts.as_ref().is_some_and(|s| {
                self.objects[i].id.is_some_and(|id| {
                    s.has_layer_binding(id, ScriptField::Origin)
                        || s.has_layer_binding(id, ScriptField::Scale)
                        || s.has_layer_binding(id, ScriptField::Angles)
                })
            });
            let local = if self.objects[i].anim.is_some() || has_local_script {
                let (o, sc, a) = self.objects[i].local_parts;
                compose_local(o, sc, a)
            } else {
                att.local
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
            let is_vis = self.is_obj_visible(i);
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
                self.draw_emitter_tree(
                    gl,
                    idx,
                    dt,
                    time,
                    ortho,
                    info.audio,
                    &info.spectrum,
                    offset,
                    pointer_world,
                    None,
                )?;
                continue;
            }

            if matches!(self.objects[i].kind, ObjectKind::Sound(_)) {
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
                        media_thumbnail: self.media_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                        media_prev_thumbnail: self.media_prev_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                    };
                    for effect in effects.iter_mut() {
                        if !effect_is_visible(effect, ctx.props, now) {
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
                        media_thumbnail: self.media_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                        media_prev_thumbnail: self.media_prev_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                    };
                    for effect in effects.iter_mut() {
                        if !effect_is_visible(effect, ctx.props, now) {
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
                        media_thumbnail: self.media_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                        media_prev_thumbnail: self.media_prev_thumb_tex.map(|(_, t, w, h)| (t, w, h)),
                    };
                    for effect in effects.iter_mut() {
                        if !effect_is_visible(effect, ctx.props, now) {
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
            if let Some(slot) = self.objects[i].user_tex.clone() {
                let override_tex = match slot {
                    UserTextureSlot::System(SystemTexture::MediaThumbnail) => {
                        self.media_thumb_tex.map(|(_, t, _, _)| t)
                    }
                    UserTextureSlot::System(SystemTexture::MediaPreviousThumbnail) => self
                        .media_prev_thumb_tex
                        .or(self.media_thumb_tex)
                        .map(|(_, t, _, _)| t),
                    UserTextureSlot::Property(ref prop_key) => prop_get(&self.properties, prop_key)
                        .and_then(|v| {
                            v.as_str()
                                .or_else(|| v.get("value").and_then(|s| s.as_str()))
                        })
                        .map(str::to_string)
                        .and_then(|path| self.textures.get_user_texture(gl, &self.pkg, &path))
                        .map(|(t, _, _)| t),
                };
                if let Some(tex) = override_tex {
                    if matches!(draw.kind, DrawKind::Image(_) | DrawKind::Solid) {
                        draw.kind = DrawKind::Image(tex);
                        draw.uv_rect = [0.0, 0.0, 1.0, 1.0];
                    }
                }
            }
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

        // Optional Wallpaper Engine bloom/HDR post-process before cover blit.
        let bloom_tex = self.render_bloom(gl)?;

        // Blit the composed scene to the surface with aspect-ratio cover scaling.
        {
            let scene_tex = self.scene.as_ref().unwrap().tex;
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
                gl.bind_texture(glow::TEXTURE_2D, Some(scene_tex));
                if let Some(bt) = bloom_tex {
                    p.set_i32(gl, "u_bloom", 1);
                    p.set_i32(gl, "u_has_bloom", 1);
                    gl.active_texture(glow::TEXTURE1);
                    gl.bind_texture(glow::TEXTURE_2D, Some(bt));
                    gl.active_texture(glow::TEXTURE0);
                } else {
                    p.set_i32(gl, "u_has_bloom", 0);
                }
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
        self.sync_sounds();
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.sync_sounds();
    }

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        match key.to_ascii_lowercase().as_str() {
            "mute" => {
                self.muted = matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                );
                self.sync_sounds();
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
            || self.scripts.as_ref().is_some_and(|s| s.uses_audio())
    }

    fn uses_media(&self) -> bool {
        self.objects.iter().any(|o| {
            matches!(o.user_tex, Some(UserTextureSlot::System(_)))
                || o.effects.iter().any(|e| e.uses_media())
        }) || self.scripts.as_ref().is_some_and(|s| s.uses_media())
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
        self.capture_and_flush_storage(true);
        // Stop scene audio before releasing GL resources.
        self.sounds.clear();
        self.sounds_started = false;
        if let Some((_, tex, _, _)) = self.media_thumb_tex.take() {
            unsafe { gl.delete_texture(tex) };
        }
        if let Some((_, tex, _, _)) = self.media_prev_thumb_tex.take() {
            unsafe { gl.delete_texture(tex) };
        }
        if let Some(canvas) = self.scene.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.scene_alt.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.scene_copy.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.bloom_a.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.bloom_b.take() {
            canvas.destroy(gl);
        }
        for canvas in self.bloom_mips.drain(..) {
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
        self.bloom_prog.destroy(gl);
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
        self.capture_and_flush_storage(true);
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

    #[test]
    fn get_layer_names_are_extracted_from_script_sources() {
        let raw = json!({
            "id": 7,
            "scale": {
                "script": "let a = thisScene.getLayer('grid_v_1');\nlet b = thisScene.getLayer(\"SharedNoise\");\nlet c = thisScene.getLayer(name);"
            },
            "alpha": { "value": 1.0 }
        });
        let mut refs = layer_script_refs(&raw);
        refs.sort();
        assert_eq!(refs, vec!["SharedNoise".to_string(), "grid_v_1".to_string()]);
        assert!(object_has_script(&raw));
        assert!(!object_has_script(&json!({"id": 1, "alpha": {"value": 1.0}})));
        assert_eq!(parent_id(&json!("42")), Some(42));
        assert_eq!(parent_id(&json!(42)), Some(42));
        assert_eq!(parent_id(&serde_json::Value::Null), None);
    }

    #[test]
    fn scripted_field_defaults_convert_units_and_user_overrides() {
        let mut props = HashMap::new();
        props.insert("spin".to_string(), json!(0.5));

        let (initial, sp) = field_script_defaults(
            ScriptField::Angles,
            &json!({
                "value": "0.00000 0.00000 1.57080",
                "scriptproperties": { "speed": { "user": "spin" }, "enabled": true }
            }),
            &props,
        );
        let v = initial.as_array().expect("vec3");
        assert!((v[2].as_f64().unwrap() - 90.0).abs() < 1e-3);
        assert_eq!(sp.get("speed"), Some(&json!(0.5)));
        assert_eq!(sp.get("enabled"), Some(&json!(true)));

        let (alpha, _) =
            field_script_defaults(ScriptField::Alpha, &json!({ "value": 0.25 }), &props);
        assert!((alpha.as_f64().unwrap() - 0.25).abs() < 1e-6);

        let (visible, _) = field_script_defaults(ScriptField::Visible, &json!({}), &props);
        assert_eq!(visible, json!(true));
    }

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

    #[test]
    fn object_has_script_and_refs_include_effects() {
        let eff_const = json!({
            "id": 99,
            "effects": [{
                "visible": { "script": "export function update() { return thisScene.getLayer('MaskLayer').visible; }" },
                "passes": [{
                    "constantshadervalues": {
                        "Color 1": { "script": "export function update() { return thisScene.getLayer('Palette').color; }" }
                    }
                }]
            }]
        });
        assert!(object_has_script(&eff_const));
        assert!(!object_enumerates_layers(&eff_const));
        assert_eq!(
            layer_script_refs(&eff_const),
            vec!["MaskLayer".to_string(), "Palette".to_string()]
        );

        let enum_obj = json!({
            "alpha": { "script": "export function update() { return thisScene.enumerateLayers().length; }" }
        });
        assert!(object_enumerates_layers(&enum_obj));
    }

    #[test]
    fn obj_anim_skips_legacy_prop_script_when_quickjs_owns_field() {
        let props = HashMap::new();
        let now = sample_time(12, 0);
        let raw = json!({
            "angles": {
                "script": "export function update(value) { value.z = engine.runtime * 20; return value; }"
            }
        });
        let mut anim = ObjAnim::parse(
            &raw,
            [0.0; 3],
            [1.0; 3],
            [0.0; 3],
            &props,
            now,
        )
        .expect("spin anim parsed");

        // When QuickJS owns `angles`, `apply_to_parts` leaves scripted parts untouched.
        let mut parts_owned = ([0.0; 3], [1.0; 3], [0.0, 0.0, 0.25]);
        let (changed, _) =
            anim.apply_to_parts(&mut parts_owned, 0.016, 2.0, 0.5, now, false, false, true);
        assert!(!changed);
        assert!((parts_owned.2[2] - 0.25).abs() < 1e-6);

        // When QuickJS does NOT own `angles`, legacy PropScript still drives it.
        let mut parts_legacy = ([0.0; 3], [1.0; 3], [0.0; 3]);
        let (changed_legacy, _) =
            anim.apply_to_parts(&mut parts_legacy, 0.016, 2.0, 0.5, now, false, false, false);
        assert!(changed_legacy);
        assert!((parts_legacy.2[2] - 40.0f32.to_radians()).abs() < 1e-4);
    }

    #[test]
    fn anim_layer_and_instance_override_scripts_discovered_and_startpaused_track_holds() {
        let puppet_with_al_script = json!({
            "id": 77,
            "animationlayers": [
                {
                    "blend": {
                        "script": "export function update() { return thisScene.getLayer('Controller').alpha; }"
                    }
                }
            ]
        });
        assert!(object_has_script(&puppet_with_al_script));
        assert_eq!(
            layer_script_refs(&puppet_with_al_script),
            vec!["Controller".to_string()]
        );

        let particle_with_io_script = json!({
            "id": 88,
            "instanceoverride": {
                "rate": {
                    "script": "export function update() { return thisScene.getLayer('Driver').scale.x; }"
                }
            }
        });
        assert!(object_has_script(&particle_with_io_script));
        assert_eq!(
            layer_script_refs(&particle_with_io_script),
            vec!["Driver".to_string()]
        );

        let props = HashMap::new();
        let now = sample_time(12, 0);
        let paused_raw = json!({
            "alpha": {
                "animation": {
                    "options": { "fps": 10, "length": 10, "mode": "single", "startpaused": true },
                    "c0": [
                        { "frame": 0, "value": 0.2, "front": { "enabled": false } },
                        { "frame": 10, "value": 1.0, "back": { "enabled": false } }
                    ]
                }
            }
        });
        let mut anim = ObjAnim::parse(&paused_raw, [0.0; 3], [1.0; 3], [0.0; 3], &props, now)
            .expect("keytracked alpha parsed");
        let mut parts = ([0.0; 3], [1.0; 3], [0.0; 3]);
        let (_, a0) = anim.apply_to_parts(&mut parts, 0.5, 0.5, 0.5, now, false, false, false);
        assert!((a0.unwrap() - 0.2).abs() < 1e-4, "paused track should hold initial value");

        anim.track_mut(ScriptField::Alpha).unwrap().play();
        let (_, a1) = anim.apply_to_parts(&mut parts, 0.5, 1.0, 0.5, now, false, false, false);
        assert!((a1.unwrap() - 0.6).abs() < 1e-4, "playing track should advance to midpoint");
    }

    #[test]
    fn raw_bool_field_respects_user_and_value() {
        let mut props = HashMap::new();
        props.insert("show_fx".to_string(), json!(false));
        let v = json!({ "user": "show_fx", "value": true, "script": "export function update() {}" });
        assert!(!raw_bool_field(Some(&v), &props));
        props.insert("show_fx".to_string(), json!(true));
        assert!(raw_bool_field(Some(&v), &props));

        let plain = json!({ "value": false, "script": "export function update() {}" });
        assert!(!raw_bool_field(Some(&plain), &props));
    }

    #[test]
    fn parse_bloom_config_handles_sdr_hdr_defaults_and_user_props() {
        let mut props = HashMap::new();
        let def_cfg = parse_bloom_config(&json!({}), &props);
        assert!(!def_cfg.enabled);
        assert!(!def_cfg.hdr);
        assert!((def_cfg.strength - 2.0).abs() < 1e-5);
        assert!((def_cfg.threshold - 0.65).abs() < 1e-5);
        assert_eq!(def_cfg.hdr_iterations, 8);

        let general = json!({
            "bloom": { "user": "u_bloom", "value": true },
            "bloomstrength": { "user": "u_str", "value": 0.5 },
            "bloomthreshold": 0.4,
            "bloomtint": { "user": "u_tint", "value": "1 1 1" },
            "hdr": { "user": "u_hdr", "value": false },
            "bloomhdrstrength": 2.4,
            "bloomhdrthreshold": 1.1,
            "bloomhdrfeather": 0.25,
            "bloomhdrscatter": 1.5,
            "bloomhdriterations": 12
        });
        props.insert("u_bloom".into(), json!(true));
        props.insert("u_str".into(), json!(1.75));
        props.insert("u_tint".into(), json!("1 0.5 0.25"));
        props.insert("u_hdr".into(), json!(true));
        let cfg = parse_bloom_config(&general, &props);
        assert!(cfg.enabled);
        assert!(cfg.hdr);
        assert!((cfg.strength - 1.75).abs() < 1e-5);
        assert!((cfg.threshold - 0.4).abs() < 1e-5);
        assert!((cfg.tint[0] - 1.0).abs() < 1e-4);
        assert!((cfg.tint[1] - 0.5).abs() < 1e-4);
        assert!((cfg.tint[2] - 0.25).abs() < 1e-4);
        assert!((cfg.hdr_strength - 2.4).abs() < 1e-5);
        assert!((cfg.hdr_threshold - 1.1).abs() < 1e-5);
        assert!((cfg.hdr_feather - 0.25).abs() < 1e-5);
        assert!((cfg.hdr_scatter - 1.5).abs() < 1e-5);
        assert_eq!(cfg.hdr_iterations, 8);

        props.insert("u_bloom".into(), json!(false));
        assert!(!parse_bloom_config(&general, &props).enabled);
    }

    #[test]
    fn scene_object_hit_test_handles_rotation_alignment_scale_and_solid() {
        let pkg = PackageFs::default();
        let _ = &pkg;
        let now = sample_time(12, 0);
        let mut obj = SceneObject {
            id: Some(1),
            name: "btn".into(),
            transform: Affine::translate(200.0, 300.0)
                .mul(Affine::rotate(std::f32::consts::FRAC_PI_2))
                .mul(Affine::scale(2.0, 1.0)),
            size: [100.0, 40.0],
            align: [0.5, 0.0],
            color: [1.0; 4],
            brightness: 1.0,
            blend: LayerBlend::Normal,
            color_blend: 0,
            parallax: [0.0, 0.0],
            visible: parse_visible(None, &HashMap::new(), now),
            solid: true,
            dependencies: Vec::new(),
            capture_composite: false,
            effects: Vec::new(),
            kind: ObjectKind::Solid,
            anim: None,
            sprite: None,
            attach: None,
            raw_idx: None,
            raw_obj: serde_json::Value::Null,
            parent_obj: None,
            local_parts: ([200.0, 300.0, 0.0], [2.0, 1.0, 1.0], [0.0, 0.0, std::f32::consts::FRAC_PI_2]),
            script_visible: None,
            script_text_override: None,
            script_sprite_frame: None,
            user_tex: None,
        };
        // With align=[0.5, 0], local_center=(50, 0); scaled by (2,1) -> (100,0);
        // rotated +90 deg -> (0, +100), so quad center is at (200, 400), covering
        // x in [180, 220] and y in [300, 500].
        let hit = obj.hit_test([205.0, 460.0], [0.0, 0.0]).expect("inside rotated scaled rect");
        assert!((hit[0] - 30.0).abs() < 1e-3);
        assert!((hit[1] + 5.0).abs() < 1e-3);
        assert!(obj.hit_test([200.0, 260.0], [0.0, 0.0]).is_none());

        // Parallax offset shifts hit test cleanly.
        assert!(obj.hit_test([215.0, 460.0], [10.0, 0.0]).is_some());

        // solid=false disables hit-testing even inside bounds.
        obj.solid = false;
        assert!(obj.hit_test([205.0, 460.0], [0.0, 0.0]).is_none());
    }

    #[test]
    fn detect_layer_media_tex_checks_instance_and_material() {
        let thumb_entry = serde_json::json!({ "name": "$mediaThumbnail", "type": "system" });
        let prev_entry =
            serde_json::json!({ "name": "$mediaPreviousThumbnail", "type": "system" });

        let obj_override = serde_json::json!({
            "instanceoverride": { "usertextures": [thumb_entry] }
        });
        assert_eq!(
            detect_layer_user_tex(&obj_override, None),
            Some(UserTextureSlot::System(SystemTexture::MediaThumbnail))
        );

        let obj_instance = serde_json::json!({
            "instance": { "usertextures": [prev_entry] }
        });
        assert_eq!(
            detect_layer_user_tex(&obj_instance, None),
            Some(UserTextureSlot::System(SystemTexture::MediaPreviousThumbnail))
        );

        let obj_plain = serde_json::json!({
            "instanceoverride": { "color": "1 1 1" }
        });
        let mat_pass = serde_json::json!({
            "shader": "genericimage4",
            "usertextures": [thumb_entry]
        });
        assert_eq!(
            detect_layer_user_tex(&obj_plain, Some(&mat_pass)),
            Some(UserTextureSlot::System(SystemTexture::MediaThumbnail))
        );

        let obj_both = serde_json::json!({
            "instanceoverride": { "usertextures": [prev_entry] },
            "instance": { "usertextures": [thumb_entry] }
        });
        assert_eq!(
            detect_layer_user_tex(&obj_both, Some(&mat_pass)),
            Some(UserTextureSlot::System(SystemTexture::MediaPreviousThumbnail))
        );

        let mat_custom = serde_json::json!({
            "shader": "genericimage4",
            "usertextures": ["newproperty"]
        });
        assert_eq!(
            detect_layer_user_tex(&obj_plain, Some(&mat_custom)),
            Some(UserTextureSlot::Property("newproperty".into()))
        );

        let obj_none = serde_json::json!({ "name": "bg" });
        assert_eq!(detect_layer_user_tex(&obj_none, None), None);
    }

    #[test]
    fn object_index_remap_preserves_order_for_move_and_remove() {
        // Move element at index 4 up behind index 1 (like sortLayer moving a shadow behind target).
        assert_eq!(
            remap_indices_after_move(5, 4, 1),
            vec![Some(0), Some(2), Some(3), Some(4), Some(1)]
        );
        // Move element down from index 1 to index 3.
        assert_eq!(
            remap_indices_after_move(5, 1, 3),
            vec![Some(0), Some(3), Some(1), Some(2), Some(4)]
        );
        // Removing index 1 drops parent pointer at 1 and shifts indices >= 2 down by 1.
        assert_eq!(
            remap_indices_after_remove(4, 1),
            vec![Some(0), None, Some(1), Some(2)]
        );
    }

    #[test]
    fn sound_layer_volume_mode_and_state_transitions() {
        let mut props = HashMap::new();
        assert!((eval_sound_volume(Some(&serde_json::json!(0.69)), &props) - 0.69).abs() < 1e-4);
        let user_vol = serde_json::json!({ "user": "sound", "value": 0.0 });
        assert!((eval_sound_volume(Some(&user_vol), &props) - 0.0).abs() < 1e-4);
        props.insert("sound".into(), serde_json::json!(0.65));
        assert!((eval_sound_volume(Some(&user_vol), &props) - 0.65).abs() < 1e-4);

        let script_user_vol = serde_json::json!({
            "script": "export function update(v) { return v; }",
            "user": "pbrvolume",
            "value": 1.0
        });
        assert!((eval_sound_volume(Some(&script_user_vol), &props) - 1.0).abs() < 1e-4);
        props.insert("pbrvolume".into(), serde_json::json!(0.42));
        assert!((eval_sound_volume(Some(&script_user_vol), &props) - 0.42).abs() < 1e-4);
        assert!((eval_sound_volume(Some(&serde_json::json!(1.5)), &props) - 1.0).abs() < 1e-4);
        assert!((eval_sound_volume(Some(&serde_json::json!(-0.2)), &props) - 0.0).abs() < 1e-4);

        assert_eq!(
            super::super::video::AudioPlayMode::parse(Some("loop")),
            super::super::video::AudioPlayMode::Loop
        );
        assert_eq!(
            super::super::video::AudioPlayMode::parse(Some("RANDOM")),
            super::super::video::AudioPlayMode::Random
        );
        assert_eq!(
            super::super::video::AudioPlayMode::parse(Some("single")),
            super::super::video::AudioPlayMode::Single
        );
        assert_eq!(
            super::super::video::AudioPlayMode::parse(None),
            super::super::video::AudioPlayMode::Loop
        );

        let mut snd = SceneSound {
            files: Vec::new(),
            mode: super::super::video::AudioPlayMode::Single,
            min_time: 1.0,
            max_time: 5.0,
            min_volume: 1.0,
            max_volume: 1.0,
            base_volume: 0.8,
            script_volume: None,
            playing: false, // e.g. startsilent: true
            script_paused: false,
            player: None,
            init_failed: false,
        };
        assert!(!snd.is_playing());
        assert!((snd.effective_layer_volume() - 0.8).abs() < 1e-4);

        snd.cmd_play(1.0, false, false, true, false);
        assert!(snd.is_playing());

        snd.cmd_pause();
        assert!(!snd.is_playing());

        snd.cmd_play(1.0, false, false, true, false);
        assert!(snd.is_playing());

        snd.cmd_stop();
        assert!(!snd.is_playing());

        snd.script_volume = Some(0.35);
        assert!((snd.effective_layer_volume() - 0.35).abs() < 1e-4);
    }

    #[test]
    fn cursor_press_capture_stops_at_visible_top_layer_but_passes_through_hidden() {
        let mut claimed = false;
        // Miss never claims.
        assert!(!claim_cursor_press(false, true, &mut claimed));
        assert!(!claimed);
        // Hidden foreground layer receives event without claiming presses meant for visible layers behind it.
        assert!(claim_cursor_press(true, false, &mut claimed));
        assert!(!claimed);
        // Visible middle layer captures the press and stops layers behind it.
        assert!(claim_cursor_press(true, true, &mut claimed));
        assert!(claimed);
        assert!(!claim_cursor_press(true, true, &mut claimed));
        assert!(!claim_cursor_press(true, false, &mut claimed));
    }

    #[test]
    fn scene_storage_path_prefers_workshop_id_and_sanitizes() {
        let base = crate::util::data_dir().join("scenescript");
        let root = std::path::Path::new("/tmp/some/wallpaper/3238423642");

        let p_num = scene_storage_path(root, Some(&serde_json::json!({ "workshopid": 3238423642u64 })));
        assert_eq!(p_num, base.join("3238423642.json"));

        let p_str = scene_storage_path(root, Some(&serde_json::json!({ "workshopid": "3238423642" })));
        assert_eq!(p_str, base.join("3238423642.json"));

        let p_trav = scene_storage_path(root, Some(&serde_json::json!({ "workshopid": "../3238423642/bad" })));
        assert_eq!(p_trav.parent(), Some(base.as_path()));
        let fname = p_trav.file_name().unwrap().to_string_lossy();
        assert!(!fname.contains("..") && !fname.contains('/'));
        assert!(fname.contains("3238423642"));

        let r1 = std::path::Path::new("/tmp/a/my scene");
        let r2 = std::path::Path::new("/tmp/b/my scene");
        let p_local1 = scene_storage_path(r1, None);
        let p_local1_again = scene_storage_path(r1, Some(&serde_json::json!({})));
        let p_local2 = scene_storage_path(r2, None);
        assert_eq!(p_local1.parent(), Some(base.as_path()));
        assert_eq!(p_local1, p_local1_again);
        assert_ne!(p_local1, p_local2);
    }

    #[test]
    fn camera_zoom_parallax_delay_and_alignment_parsing() {
        assert_eq!(parse_alignment_str("center"), [0.0, 0.0]);
        assert_eq!(parse_alignment_str("left"), [0.5, 0.0]);
        assert_eq!(parse_alignment_str("right"), [-0.5, 0.0]);
        assert_eq!(parse_alignment_str("top"), [0.0, -0.5]);
        assert_eq!(parse_alignment_str("bottom"), [0.0, 0.5]);
        assert_eq!(parse_alignment_str("topleft"), [0.5, -0.5]);
        assert_eq!(parse_alignment_str("bottomright"), [-0.5, 0.5]);

        let fast = parallax_smoothing_factor(1.0 / 60.0, 0.1);
        let slow = parallax_smoothing_factor(1.0 / 60.0, 2.0);
        assert!(fast > slow && slow > 0.0);
        assert_eq!(parallax_smoothing_factor(0.0, 0.1), 0.0);

        let (s1, o1) = cover_uv_with_zoom([1920.0, 1080.0], [1920, 1080], 1.0);
        assert!((s1[0] - 1.0).abs() < 1e-5 && (s1[1] - 1.0).abs() < 1e-5);
        assert!(o1[0].abs() < 1e-5 && o1[1].abs() < 1e-5);

        let (s2, o2) = cover_uv_with_zoom([1920.0, 1080.0], [1920, 1080], 2.0);
        assert!((s2[0] - 0.5).abs() < 1e-5 && (s2[1] - 0.5).abs() < 1e-5);
        assert!((o2[0] - 0.25).abs() < 1e-5 && (o2[1] - 0.25).abs() < 1e-5);
    }
}
