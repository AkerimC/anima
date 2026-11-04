//! Anima Engine 2D Desktop Character / Mascot / Companion Renderer.
//!
//! Renders an interactive 2D character anchored to any corner/edge of the screen:
//! * Built-in procedural **Reze** (*Chainsaw Man*) character (`anima char reze`):
//!   - Dark violet shoulder-length bob (no bun) with long parted bangs that hide the
//!     eyebrows, one strand falling between the eyes, and short physics-swaying
//!     sidelocks that curl inward at the jaw
//!   - Soft, slightly droopy emerald-green anime eyes that smoothly follow the mouse
//!     cursor and blink naturally, plus her trademark heavy blush
//!   - Signature black neck choker with the silver grenade pull-pin and ring
//!   - White sleeveless collared blouse with a small black bow and bare shoulders
//!   - Audio-reactive lip-sync, breathing, head tilt, and subtle Bomb Devil spark
//!     particles on music beats
//! * Additional procedural presets (`neko`, `cyber`, `spirit`)
//! * Custom character sprites (`.png`, `.gif`, `.webp`, `.tex`) with Live2D-style
//!   2.5D procedural breathing, mouse-look tilt, and audio bounce.

use std::time::Instant;

use anyhow::Result;
use glow::HasContext;
use serde::{Deserialize, Serialize};

use super::FrameInfo;
use super::gl::{Filter, Program, create_texture};
use super::image::ImageRenderer;
use crate::pkg::decode_tex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
    BottomCenter,
}

impl Default for Corner {
    fn default() -> Self {
        Corner::BottomRight
    }
}

impl Corner {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "bottom-left" | "bl" | "left" | "sol-alt" | "sol" => Corner::BottomLeft,
            "top-right" | "tr" | "sag-ust" => Corner::TopRight,
            "top-left" | "tl" | "sol-ust" => Corner::TopLeft,
            "bottom-center" | "bc" | "center" | "orta" => Corner::BottomCenter,
            _ => Corner::BottomRight,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Corner::BottomRight => "bottom-right",
            Corner::BottomLeft => "bottom-left",
            Corner::TopRight => "top-right",
            Corner::TopLeft => "top-left",
            Corner::BottomCenter => "bottom-center",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Placement {
    /// Drawn on the wallpaper layer, behind desktop windows.
    Wallpaper,
    /// Drawn in front of all windows, anchored to the screen corner.
    ScreenFront,
    /// Drawn in front of windows, tucked inside the active window's corner.
    WindowInside,
    /// Drawn in front of windows, perched on the outside edge/corner of the active window.
    WindowOutside,
}

impl Default for Placement {
    fn default() -> Self {
        Placement::Wallpaper
    }
}

impl Placement {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "screen-front" | "front" | "overlay" | "on-top" | "ontop" | "on" | "ön" | "onde" => {
                Placement::ScreenFront
            }
            "window-inside" | "inside" | "win-in" | "in-window" | "pencere-ici" | "ic" => {
                Placement::WindowInside
            }
            "window-outside" | "outside" | "win-out" | "out-window" | "on-window"
            | "pencere-disi" | "dis" | "ust" => Placement::WindowOutside,
            _ => Placement::Wallpaper,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Wallpaper => "wallpaper",
            Placement::ScreenFront => "screen-front",
            Placement::WindowInside => "window-inside",
            Placement::WindowOutside => "window-outside",
        }
    }

    pub fn is_front_overlay(self) -> bool {
        !matches!(self, Placement::Wallpaper)
    }

    pub fn needs_overlay_surface(self) -> bool {
        !matches!(self, Placement::Wallpaper)
    }
}

/// Active window geometry in logical compositor coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CharacterConfig {
    /// Enabled flag.
    pub enabled: bool,
    /// Path to a `.png`/`.gif`/`.webp` sprite, or a preset (`reze`, `neko`, `cyber`, `spirit`).
    pub source: String,
    /// Corner anchor (on screen or on the active window).
    pub corner: Corner,
    /// Where to place the character (`wallpaper`, `screen-front`, `window-inside`, `window-outside`).
    pub placement: Placement,
    /// Character size scale (`0.2 .. 4.0`, where `1.0` is ~360px tall).
    pub scale: f32,
    /// Pixel offset from the anchored corner `(x, y)`.
    pub offset: [f32; 2],
    /// Opacity (`0.0 .. 1.0`).
    pub opacity: f32,
    /// Mirror character horizontally.
    pub flip_x: bool,
    /// Track mouse cursor with head/eyes/tilt.
    pub look_at_mouse: bool,
    /// Bounce and lip-sync to desktop audio / music.
    pub audio_reactive: bool,
    /// Legacy alias: when `true` and `placement == Wallpaper`, upgrades to `ScreenFront`.
    pub always_on_top: bool,
    /// Frame rate cap used when the character animates over a *static*
    /// wallpaper. Animated wallpapers keep running at the wallpaper fps.
    pub fps: u32,
}

impl CharacterConfig {
    pub fn effective_placement(&self) -> Placement {
        if self.placement == Placement::Wallpaper && self.always_on_top {
            Placement::ScreenFront
        } else {
            self.placement
        }
    }
}

impl Default for CharacterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: "reze".into(),
            corner: Corner::BottomRight,
            placement: Placement::Wallpaper,
            scale: 1.0,
            offset: [24.0, 0.0],
            opacity: 1.0,
            flip_x: false,
            look_at_mouse: true,
            audio_reactive: true,
            always_on_top: false,
            fps: 30,
        }
    }
}

const CHAR_VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos; // -1..1
out vec2 v_uv;

uniform vec2 u_center;    // clip space center
uniform vec2 u_half_size; // clip space half size
uniform vec2 u_tilt;      // Live2D-style mouse look skew/tilt
uniform float u_breath;   // breathing scale Y
uniform float u_bounce;   // audio bounce Y offset
uniform int u_flip_x;

void main() {
    vec2 uv = a_pos * 0.5 + 0.5;
    if (u_flip_x == 1) {
        uv.x = 1.0 - uv.x;
    }
    v_uv = uv;

    // Anchor bottom of character (-1.0) so breathing & bounce happen from waist up
    float height_factor = (a_pos.y + 1.0) * 0.5;
    vec2 p = a_pos;
    p.y = -1.0 + (p.y + 1.0) * (1.0 + u_breath * 0.025);
    p.x += height_factor * u_tilt.x * 0.055;
    p.y += height_factor * u_tilt.y * 0.025;

    vec2 clip = u_center + p * u_half_size;
    clip.y += u_bounce;
    gl_Position = vec4(clip, 0.0, 1.0);
}
"#;

const CHAR_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;

uniform sampler2D u_tex;
// 0 = custom texture/sprite, 1 = reze, 2 = neko, 3 = cyber, 4 = spirit
uniform int u_mode;
uniform float u_time;
uniform float u_opacity;
uniform vec2 u_look;  // normalized mouse direction from character (-1..1)
uniform vec4 u_audio; // bass, mid, high, level
uniform float u_beat;
uniform vec4 u_mouth; // x=open(0..1), y=form(-1..1: -1=O/U, 0=A, +1=I/E), z=sibilance(0..1), w=active(0..1)

out vec4 fragColor;

float sdCircle(vec2 p, float r) {
    return length(p) - r;
}

float sdEllipse(vec2 p, vec2 r) {
    return (length(p / r) - 1.0) * min(r.x, r.y);
}

float sdSegment(vec2 p, vec2 a, vec2 b) {
    vec2 pa = p - a, ba = b - a;
    float h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

float sdRoundedBox(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

float sdTriangleIsosceles(vec2 p, vec2 q) {
    p.x = abs(p.x);
    vec2 a = p - q * clamp(dot(p, q) / dot(q, q), 0.0, 1.0);
    vec2 b = p - q * vec2(clamp(p.x / q.x, 0.0, 1.0), 1.0);
    float s = -sign(q.y);
    vec2 d = min(vec2(dot(a, a), s * (p.x * q.y - p.y * q.x)),
                  vec2(dot(b, b), s * (p.y - q.y)));
    return -sqrt(d.x) * sign(d.y);
}

void blendOver(inout vec4 dst, vec3 rgb, float alpha) {
    float a = clamp(alpha, 0.0, 1.0);
    dst.rgb = mix(dst.rgb, rgb, a);
    dst.a = max(dst.a, a);
}

// ============================================================================
// REZE (Chainsaw Man — Bomb Girl Arc) Procedural Live2D-style Shader
// ============================================================================
vec4 render_reze(vec2 uv) {
    // Coordinate space: x in -0.85..0.85, y in -1.0..1.0 (bottom is -1.0)
    vec2 p = (uv * 2.0 - 1.0) * vec2(0.88, 1.0);

    float sway = sin(u_time * 1.9) * 0.018 + u_look.x * 0.045;
    float head_bob = sin(u_time * 3.1) * 0.012 + u_beat * 0.025;
    vec2 hp = p - vec2(sway, head_bob);

    // Palette inspired by MAPPA's Chainsaw Man: Reze Arc
    vec3 hairDark   = vec3(0.13, 0.10, 0.23); // Near-black indigo shadow
    vec3 hairBase   = vec3(0.30, 0.23, 0.44); // Dark violet-purple base (not pastel)
    vec3 hairLight  = vec3(0.50, 0.40, 0.72); // Soft lavender shine
    vec3 skinBase   = vec3(0.99, 0.92, 0.89); // Soft warm porcelain skin
    vec3 skinShadow = vec3(0.93, 0.80, 0.78); // Neck/chin soft anime shading
    vec3 eyeEmerald = vec3(0.12, 0.78, 0.52); // Vibrant emerald green iris
    vec3 eyeDeep    = vec3(0.04, 0.36, 0.28); // Upper iris emerald shadow
    vec3 eyeGlow    = vec3(0.55, 0.98, 0.74); // Lower iris bright green highlight
    vec3 shirtWhite = vec3(0.96, 0.97, 0.99);
    vec3 shirtShade = vec3(0.84, 0.86, 0.92);
    vec3 ribbonDark = vec3(0.11, 0.11, 0.15);
    vec3 outlineCol = vec3(0.12, 0.08, 0.16);

    vec4 col = vec4(0.0);

    // 0. Subtle Bomb Devil spark / fuse aura when audio beat or bass plays
    float spark_intensity = clamp(u_audio.x * 0.85 + u_beat * 1.1, 0.0, 1.0);
    if (spark_intensity > 0.02) {
        for (float i = 0.0; i < 6.0; i += 1.0) {
            float ang = i * 1.047 + u_time * (1.4 + i * 0.2);
            float rad = 0.48 + 0.12 * sin(u_time * 3.0 + i * 2.1);
            vec2 sp = hp - vec2(cos(ang) * rad * 0.85, sin(ang) * rad + 0.05);
            float d = length(sp);
            float glow = exp(-d * 28.0) * spark_intensity;
            vec3 sparkCol = mix(vec3(1.0, 0.38, 0.12), vec3(1.0, 0.82, 0.35), fract(i * 0.37));
            col.rgb += sparkCol * glow * 0.8;
            col.a = max(col.a, glow * 0.7);
        }
    }

    // 1. Back hair: Reze's shoulder-length bob (no bun). The ends tuck in toward the neck.
    float d_dome = sdEllipse(hp - vec2(0.0, 0.26), vec2(0.37, 0.32));
    vec2 bob_p = hp - vec2(0.0, -0.10);
    float bob_taper = smoothstep(0.10, -0.25, bob_p.y);
    bob_p.x *= 1.0 + 0.20 * bob_taper;
    float d_bob = min(d_dome, sdRoundedBox(bob_p, vec2(0.37, 0.25), 0.13));
    blendOver(col, hairDark, smoothstep(0.008, -0.006, d_bob));

    // 2. Bare shoulders, neck & sleeveless torso (anchored at bottom y = -1.0)
    vec2 body_p = p - vec2(sway * 0.45, -0.72);

    // Bare shoulders (soft rounded capsule across shoulders)
    float d_shoulders = sdRoundedBox(body_p - vec2(0.0, 0.02), vec2(0.41, 0.32), 0.18);
    if (d_shoulders < 0.01) {
        float sa = smoothstep(0.008, -0.006, d_shoulders);
        vec3 shCol = mix(skinBase, skinShadow, smoothstep(-0.1, 0.25, abs(body_p.x) - 0.18));
        blendOver(col, shCol, sa);
        // Subtle collarbone lines
        float collar_line = sdSegment(vec2(abs(body_p.x), body_p.y), vec2(0.07, 0.20), vec2(0.24, 0.23));
        blendOver(col, skinShadow, smoothstep(0.009, 0.002, collar_line) * 0.65 * sa);
    }

    // Slender neck + chin shadow
    vec2 neck_p = p - vec2(sway * 0.75, -0.25);
    float d_neck = sdRoundedBox(neck_p, vec2(0.105, 0.19), 0.04);
    if (d_neck < 0.01) {
        float na = smoothstep(0.008, -0.006, d_neck);
        // Cast shadow under chin
        float chin_sh = smoothstep(0.01, 0.12, neck_p.y);
        vec3 nCol = mix(skinBase, skinShadow, chin_sh * 0.85);
        blendOver(col, nCol, na);
    }

    // 3. Reze's signature black choker with the grenade pull-pin and ring
    vec2 choker_p = neck_p - vec2(0.0, -0.075);
    float d_choker = sdRoundedBox(choker_p, vec2(0.112, 0.030), 0.012);
    blendOver(col, ribbonDark, smoothstep(0.007, -0.005, d_choker));

    // Pin shaft threaded through the front of the choker, ring dangling on the collarbone
    vec3 silver = vec3(0.85, 0.89, 0.95) + vec3(1.0, 0.65, 0.25) * spark_intensity * 0.6;
    vec2 pin_p = choker_p - vec2(0.052, 0.0);
    float d_pin = sdSegment(pin_p, vec2(0.0, 0.004), vec2(0.004, -0.048)) - 0.0045;
    blendOver(col, vec3(0.70, 0.74, 0.82), smoothstep(0.005, -0.003, d_pin));
    vec2 ring_p = pin_p - vec2(0.006, -0.076);
    float d_ring = abs(sdCircle(ring_p, 0.028)) - 0.0055;
    blendOver(col, silver, smoothstep(0.005, -0.003, d_ring));

    // 4. White Sleeveless Collared Blouse + Black Ribbon Tie
    float d_shirt = sdRoundedBox(body_p - vec2(0.0, -0.03), vec2(0.28, 0.33), 0.08);
    if (d_shirt < 0.01) {
        float sha = smoothstep(0.008, -0.006, d_shirt);
        vec3 sCol = mix(shirtWhite, shirtShade, smoothstep(0.12, 0.28, abs(body_p.x)));
        blendOver(col, sCol, sha);

        // Crisp V-neck collar flaps
        vec2 col_p = vec2(abs(body_p.x), body_p.y - 0.22);
        float d_flap = sdTriangleIsosceles(col_p - vec2(0.08, 0.05), vec2(0.085, -0.14));
        blendOver(col, shirtWhite, smoothstep(0.007, -0.005, d_flap));
        blendOver(col, shirtShade, smoothstep(0.006, 0.0, abs(d_flap) - 0.003) * 0.7);

        // Small black bow & short tails at the collar
        vec2 rib_p = body_p - vec2(0.0, 0.22);
        // Knot
        float d_knot = sdRoundedBox(rib_p, vec2(0.028, 0.026), 0.01);
        // Bow loops
        vec2 loop_p = vec2(abs(rib_p.x) - 0.065, rib_p.y + rib_p.x * sign(rib_p.x) * 0.15);
        float d_loop = sdRoundedBox(loop_p, vec2(0.055, 0.026), 0.012);
        // Two hanging black ribbon tails swaying slightly with music/movement
        vec2 tail_p = rib_p - vec2(sin(u_time * 2.5 + rib_p.y * 6.0) * 0.012, 0.0);
        float d_tail_l = sdSegment(tail_p, vec2(-0.015, -0.01), vec2(-0.045, -0.17));
        float d_tail_r = sdSegment(tail_p, vec2( 0.015, -0.01), vec2( 0.045, -0.15));
        float d_ribbon = min(min(d_knot, d_loop), min(d_tail_l - 0.014, d_tail_r - 0.014));
        blendOver(col, ribbonDark, smoothstep(0.007, -0.005, d_ribbon) * sha);
    }

    // 5. Head & Anime Face Contour (tapered chin + soft cheeks + dynamic jaw movement)
    vec2 face_p = hp - vec2(0.0, 0.14);
    float jaw_drop = u_mouth.x * 0.018;
    vec2 jaw_face_p = face_p;
    if (jaw_face_p.y < -0.04) {
        jaw_face_p.y += jaw_drop * smoothstep(-0.04, -0.26, jaw_face_p.y);
    }
    // Taper width towards chin for an authentic anime jawline
    float jaw_taper = clamp((jaw_face_p.y + 0.30) * 1.4, 0.0, 1.0);
    vec2 shaped_face = vec2(jaw_face_p.x / mix(0.68, 1.02, jaw_taper), jaw_face_p.y);
    float d_head = sdRoundedBox(shaped_face, vec2(0.31, 0.29), 0.22);

    if (d_head < 0.01) {
        float ha = smoothstep(0.008, -0.006, d_head);
        vec3 fCol = mix(skinShadow, skinBase, smoothstep(-0.28, -0.14, face_p.y));
        blendOver(col, fCol, ha);
        // Thin anime jawline contour
        blendOver(col, outlineCol, smoothstep(0.006, 0.0, abs(d_head) - 0.003) * 0.45);

        // Soft warm cheek blush
        vec2 blush_p = vec2(abs(face_p.x) - 0.165, face_p.y + 0.045);
        float blush = exp(-dot(blush_p * vec2(1.0, 1.8), blush_p * vec2(1.0, 1.8)) * 190.0);
        col.rgb = mix(col.rgb, vec3(0.98, 0.52, 0.58), blush * (0.55 + u_beat * 0.25) * ha);

        // Subtle anime nose shadow/dot
        vec2 nose_p = face_p - vec2(u_look.x * 0.015, -0.045);
        float d_nose = sdSegment(nose_p, vec2(0.0, 0.012), vec2(-0.006, -0.008));
        blendOver(col, skinShadow * 0.85, smoothstep(0.007, 0.001, d_nose) * 0.85);

        // 6. Expressive Emerald Green Eyes with Mouse Tracking & Blinking
        float blink_cycle = mod(u_time, 4.5);
        float blink = smoothstep(0.0, 0.065, abs(blink_cycle - 4.1));
        vec2 look_shift = clamp(u_look, vec2(-1.0), vec2(1.0)) * vec2(0.032, 0.022);

        float side = sign(face_p.x);
        vec2 ep = vec2(abs(face_p.x) - 0.135, face_p.y - 0.005);
        ep.y += ep.x * 0.25; // soft droopy (tareme) outer corners
        float eye_h = max(0.005, 0.060 * blink);
        float d_sclera = sdRoundedBox(ep, vec2(0.062, eye_h), 0.030);

        if (d_sclera < 0.006 && blink > 0.18) {
            float ea = smoothstep(0.006, -0.005, d_sclera);
            // White sclera with subtle top lash shadow
            vec3 scleraCol = mix(vec3(0.98, 0.99, 1.0), vec3(0.84, 0.85, 0.92), smoothstep(0.0, eye_h, ep.y));
            blendOver(col, scleraCol, ea);

            // Emerald green iris following mouse cursor
            vec2 iris_center = vec2(side * look_shift.x, look_shift.y);
            vec2 ip = ep - iris_center;
            float d_iris = sdEllipse(ip, vec2(0.042, 0.050));
            if (d_iris < 0.005) {
                float ia = smoothstep(0.005, -0.004, d_iris) * ea;
                // Gradient from deep emerald at top to bright jade at bottom
                float igrad = clamp((ip.y + 0.045) / 0.09, 0.0, 1.0);
                vec3 irisCol = mix(eyeGlow, mix(eyeEmerald, eyeDeep, igrad), igrad);
                blendOver(col, irisCol, ia);

                // Dark pupil
                float d_pupil = sdCircle(ip, 0.017);
                blendOver(col, vec3(0.03, 0.14, 0.12), smoothstep(0.005, -0.003, d_pupil) * ia);

                // Dual anime catchlights
                float hl1 = sdCircle(ip - vec2(-0.015 * side, 0.018), 0.012);
                float hl2 = sdCircle(ip - vec2( 0.014 * side, -0.016), 0.006);
                blendOver(col, vec3(1.0), smoothstep(0.004, -0.003, min(hl1, hl2)) * ia);
            }
        }

        // Upper thick dark-purple eyelash arch
        vec2 lash_p = ep - vec2(0.0, eye_h - 0.004);
        lash_p.y += (lash_p.x * lash_p.x) * 3.2;
        float d_lash = sdRoundedBox(lash_p, vec2(0.068, 0.011), 0.006);
        blendOver(col, outlineCol, smoothstep(0.006, -0.004, d_lash));

        // Delicate arched eyebrows
        vec2 brow_p = ep - vec2(0.0, 0.088 + u_beat * 0.008);
        brow_p.y += (brow_p.x + 0.01) * (brow_p.x + 0.01) * 2.5;
        float d_brow = sdRoundedBox(brow_p, vec2(0.060, 0.005), 0.003);
        blendOver(col, hairDark, smoothstep(0.005, -0.003, d_brow) * 0.85);

        // 7. Multi-Viseme (A, I, U, E, O + M/B/P Closure + Sibilant Teeth) Mouth
        float m_open = clamp(u_mouth.x, 0.0, 1.0);
        float m_form = clamp(u_mouth.y, -1.0, 1.0); // -1 = rounded O/U, 0 = open A, +1 = spread I/E
        float m_sib  = clamp(u_mouth.z, 0.0, 1.0);
        float m_act  = clamp(u_mouth.w, 0.0, 1.0);

        vec2 mp = face_p - vec2(look_shift.x * 0.35, -0.145 - jaw_drop * 0.45);
        if (m_open > 0.035) {
            // Width expands on spread vowels (I, E) and narrows on rounded vowels (O, U)
            float mw = clamp(0.032 + m_form * 0.015 + m_open * 0.007, 0.016, 0.052);
            // Height opens on A/O/E and compresses slightly on close vowels (I) or sibilants (S/Ş)
            float mh = clamp(0.004 + m_open * 0.040 * (1.0 - max(m_form, 0.0) * 0.22), 0.005, 0.045);

            // Shape distortion: smile arch for I/E, neutral trapezoid for A, rounded oval for O/U
            vec2 q = mp;
            q.y -= (q.x * q.x) * (m_form * 2.8);
            float roundness = clamp(0.010 + max(-m_form, 0.0) * 0.012 + m_open * 0.008, 0.004, min(mw, mh));
            float d_mouth = mix(
                sdRoundedBox(q, vec2(mw, mh), roundness),
                sdEllipse(q, vec2(mw, mh)),
                clamp(-m_form * 0.85, 0.0, 0.85)
            );

            if (d_mouth < 0.005) {
                float ma = smoothstep(0.004, -0.003, d_mouth);
                // Deep oral cavity base
                vec3 mCol = vec3(0.36, 0.09, 0.15);

                // Warm coral-pink tongue in the lower oral cavity
                vec2 tp = q - vec2(m_form * 0.003, -mh * 0.55);
                float d_tongue = sdEllipse(tp, vec2(mw * 0.78, mh * 0.72));
                if (d_tongue < 0.0) {
                    vec3 tongueCol = mix(vec3(0.94, 0.44, 0.52), vec3(0.76, 0.26, 0.36), clamp(-tp.y * 18.0, 0.0, 1.0));
                    mCol = mix(mCol, tongueCol, smoothstep(0.003, -0.003, d_tongue));
                }

                // Crisp white upper teeth row along top of mouth (especially prominent on I/E & sibilants S/Ş/Ç)
                float teeth_drop = mh * (0.52 - 0.28 * max(m_sib, max(m_form, 0.0)));
                float d_teeth = q.y - teeth_drop;
                if (d_teeth > -0.005 && abs(q.x) < mw * 0.86) {
                    float ta = smoothstep(-0.003, 0.002, d_teeth);
                    mCol = mix(mCol, vec3(0.98, 0.97, 0.99), ta);
                }

                blendOver(col, mCol, ma);
                // Crisp anime lip outline
                blendOver(col, vec3(0.32, 0.14, 0.20), smoothstep(0.0045, 0.0, abs(d_mouth) - 0.0018) * 0.85);
            }
            // Subtle lower-lip gloss highlight
            vec2 lip_hl = mp - vec2(0.0, -mh - 0.005);
            float d_lip_hl = sdSegment(lip_hl, vec2(-mw * 0.38, 0.0), vec2(mw * 0.38, 0.0));
            blendOver(col, skinShadow * 0.88, smoothstep(0.005, 0.001, d_lip_hl) * 0.55);
        } else if (m_act > 0.22) {
            // Bilabial consonant closure (M / B / P): lips pressed neatly together mid-phrase
            vec2 bp_m = mp;
            float d_closed = sdSegment(bp_m, vec2(-0.027, 0.0), vec2(0.027, 0.0));
            blendOver(col, vec3(0.40, 0.20, 0.26), smoothstep(0.0055, 0.001, d_closed));
            float d_phil = sdSegment(bp_m - vec2(0.0, -0.006), vec2(-0.010, 0.0), vec2(0.010, 0.0));
            blendOver(col, skinShadow * 0.86, smoothstep(0.0045, 0.001, d_phil) * 0.55);
        } else {
            // Reze's signature gentle curved smile with a tiny playful corner when idle
            vec2 sp = mp;
            sp.y -= (sp.x * sp.x) * 4.5;
            float d_smile = sdSegment(sp, vec2(-0.036, 0.0), vec2(0.038, 0.003));
            blendOver(col, vec3(0.42, 0.22, 0.28), smoothstep(0.006, 0.001, d_smile));
        }
    }

    // 8. Hair in front of the face: long parted bangs + short bob sidelocks
    // Crown / top hair dome. Its lower edge hides the eyebrows (hair-over-brows look).
    vec2 crown_p = hp - vec2(0.0, 0.36);
    float d_crown = sdEllipse(crown_p, vec2(0.36, 0.16));
    if (d_crown < 0.008) {
        float ca = smoothstep(0.007, -0.005, d_crown);
        float shine = exp(-abs(crown_p.y + 0.02 + crown_p.x * crown_p.x * 1.4) * 38.0);
        vec3 cCol = mix(hairBase, hairLight, shine * 0.45);
        blendOver(col, cCol, ca);
    }

    // Long wispy bangs with the part just left of center. Tips point down and sweep
    // away from the part; the center strand falls between the eyes.
    for (float i = -3.0; i <= 3.0; i += 1.0) {
        float bx = i * 0.072 - 0.02;
        float dir = (bx > -0.03) ? 1.0 : -1.0;
        float len = 0.135 + sin(i * 2.7) * 0.012;
        if (i == 0.0) {
            len = 0.20;
        } else if (abs(i) > 2.5) {
            len = 0.175;
        }
        float sway_b = sin(u_time * 2.2 + i * 0.7) * 0.006 + u_look.x * 0.012;
        vec2 bp = hp - vec2(bx + sway_b, 0.31 - len); // origin = strand tip
        float depth_b = clamp(1.0 - bp.y / len, 0.0, 1.0);
        bp.x -= dir * depth_b * depth_b * 0.03;
        float d_strand = sdTriangleIsosceles(bp, vec2(0.048, len)); // tip down
        if (d_strand < 0.007) {
            float sa = smoothstep(0.006, -0.005, d_strand);
            vec3 bCol = mix(hairBase, hairDark, depth_b * 0.55);
            blendOver(col, bCol, sa);
        }
    }

    // Short bob sidelocks: end around the jaw/neck and curl inward, swaying slightly
    for (float s = -1.0; s <= 1.0; s += 2.0) {
        vec2 lp = hp - vec2(s * 0.295, 0.30);
        float depth = clamp(-lp.y / 0.56, 0.0, 1.0);
        float lock_sway = (sin(u_time * 2.4 + s * 0.8) * 0.020 + u_look.x * 0.030) * depth * depth;
        lp.x += s * depth * depth * 0.06; // inward curl at the ends
        lp.x -= lock_sway;
        float lock_r = mix(0.075, 0.040, depth);
        float d_lock = sdSegment(lp, vec2(0.0, 0.0), vec2(0.0, -0.56)) - lock_r;
        if (d_lock < 0.007) {
            float la = smoothstep(0.006, -0.005, d_lock);
            float h_shine = exp(-abs(lp.y + 0.18) * 22.0) * 0.45;
            vec3 lCol = mix(mix(hairBase, hairDark, depth * 0.7), hairLight, h_shine);
            blendOver(col, lCol, la);
        }
    }

    col.a *= u_opacity;
    return col;
}

vec4 render_neko(vec2 uv, int style) {
    vec2 p = uv * 2.0 - 1.0;
    float sway = sin(u_time * 2.2) * 0.025 + u_look.x * 0.05;
    vec2 hp = p - vec2(sway, sin(u_time * 3.2) * 0.018 + u_beat * 0.035);

    vec3 primary = (style == 3) ? vec3(0.18, 0.85, 0.98)
                 : (style == 4) ? vec3(0.62, 0.42, 1.00)
                                : vec3(0.96, 0.56, 0.76);
    vec3 hairCol = (style == 3) ? vec3(0.10, 0.16, 0.28)
                 : (style == 4) ? vec3(0.18, 0.12, 0.32)
                                : vec3(0.18, 0.15, 0.24);
    vec3 skinCol = vec3(1.0, 0.93, 0.90);

    vec4 col = vec4(0.0);

    float aura_r = length(hp - vec2(0.0, -0.05));
    float aura = exp(-abs(aura_r - 0.58 - u_audio.x * 0.08) * 18.0) * (0.15 + u_audio.w * 0.75);
    col.rgb += primary * aura;
    col.a = max(col.a, aura * 0.65);

    float back_hair = sdRoundedBox(hp - vec2(0.0, 0.02), vec2(0.42, 0.46), 0.24);
    if (back_hair < 0.01) {
        col = mix(col, vec4(hairCol, 1.0), smoothstep(0.01, -0.01, back_hair));
    }

    vec2 ear_p = vec2(abs(hp.x) - 0.26, hp.y - 0.62);
    ear_p.x += ear_p.y * 0.18;
    float ear_outer = sdTriangleIsosceles(ear_p, vec2(0.18, -0.28));
    if (ear_outer < 0.01) {
        col = mix(col, vec4(hairCol, 1.0), smoothstep(0.01, -0.01, ear_outer));
        float ear_inner = sdTriangleIsosceles(ear_p - vec2(0.0, -0.04), vec2(0.10, -0.18));
        if (ear_inner < 0.01) {
            col = mix(col, vec4(primary * (0.85 + u_audio.x * 0.4), 1.0), smoothstep(0.01, -0.01, ear_inner));
        }
    }

    vec2 body_p = p - vec2(sway * 0.5, -0.68);
    float body = sdRoundedBox(body_p, vec2(0.34, 0.36), 0.16);
    if (body < 0.01) {
        vec3 outfit = mix(hairCol * 0.85, primary, 0.25);
        col = mix(col, vec4(outfit, 1.0), smoothstep(0.01, -0.01, body));
    }

    vec2 face_p = hp - vec2(0.0, 0.10);
    float head = sdRoundedBox(face_p, vec2(0.35, 0.31), 0.24);
    if (head < 0.01) {
        col = mix(col, vec4(skinCol, 1.0), smoothstep(0.01, -0.01, head));
        float blink_cycle = mod(u_time, 4.2);
        float blink = smoothstep(0.0, 0.07, abs(blink_cycle - 3.9));
        vec2 look_offset = clamp(u_look, vec2(-1.0), vec2(1.0)) * vec2(0.038, 0.028);
        vec2 ep = vec2(abs(face_p.x) - 0.145, face_p.y + 0.01);
        float eye_box = sdRoundedBox(ep, vec2(0.058, max(0.008, 0.075 * blink)), 0.028);
        if (eye_box < 0.008) {
            col.rgb = mix(col.rgb, vec3(0.12, 0.10, 0.18), smoothstep(0.008, -0.008, eye_box));
            if (blink > 0.25) {
                vec2 side_look = vec2(face_p.x > 0.0 ? look_offset.x : -look_offset.x, look_offset.y);
                float iris = sdCircle(ep - side_look, 0.042);
                if (iris < 0.006) {
                    col.rgb = mix(col.rgb, primary, smoothstep(0.006, -0.006, iris));
                }
            }
        }
        vec2 bang_p = face_p - vec2(0.0, 0.22);
        float bang = sdRoundedBox(bang_p, vec2(0.36, 0.11), 0.06) + sin(face_p.x * 18.0) * 0.035;
        if (bang < 0.01) {
            col.rgb = mix(col.rgb, hairCol, smoothstep(0.01, -0.01, bang));
        }
    }

    col.a *= u_opacity;
    return col;
}

void main() {
    vec4 c;
    if (u_mode == 0) {
        vec2 uv = vec2(v_uv.x, 1.0 - v_uv.y);
        c = texture(u_tex, clamp(uv, 0.0, 1.0));
        c.a *= u_opacity;
    } else if (u_mode == 1) {
        c = render_reze(v_uv);
    } else {
        c = render_neko(v_uv, u_mode);
    }
    // Premultiplied alpha output so transparent Wayland overlay surfaces composite cleanly
    fragColor = vec4(c.rgb * c.a, c.a);
}
"#;

pub struct CharacterOverlay {
    pub config: CharacterConfig,
    program: Program,
    vao: glow::VertexArray,
    #[allow(dead_code)]
    vbo: glow::Buffer,
    dummy_tex: glow::Texture,
    sprite_renderer: Option<ImageRenderer>,
    sprite_tex: Option<(glow::Texture, u32, u32)>,
    smooth_look: [f32; 2],
    smooth_bounce: f32,
    smooth_pos: Option<[f32; 2]>,
    start: Instant,
}

impl CharacterOverlay {
    pub fn new(gl: &glow::Context, config: CharacterConfig, width: i32, height: i32) -> Result<Self> {
        let program = Program::new(gl, CHAR_VS, CHAR_FS)?;
        let (vao, vbo) = create_quad(gl)?;
        let dummy_tex = create_texture(gl, 1, 1, &[0, 0, 0, 0], Filter::Nearest, false)?;

        let mut this = Self {
            config: config.clone(),
            program,
            vao,
            vbo,
            dummy_tex,
            sprite_renderer: None,
            sprite_tex: None,
            smooth_look: [0.0, 0.0],
            smooth_bounce: 0.0,
            smooth_pos: None,
            start: Instant::now(),
        };
        this.reload_source(gl, width, height)?;
        Ok(this)
    }

    pub fn reload_source(&mut self, gl: &glow::Context, _width: i32, _height: i32) -> Result<()> {
        self.sprite_tex = None;
        self.sprite_renderer = None;

        let src = self.config.source.trim();
        if matches!(
            src.to_ascii_lowercase().as_str(),
            "" | "default" | "reze" | "neko" | "cyber" | "spirit"
        ) {
            return Ok(());
        }

        let path = crate::util::expand_path(src, std::env::current_dir().ok().as_deref());
        if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let img = if ext == "tex" {
                let bytes = std::fs::read(&path)?;
                decode_tex(&bytes)?.image
            } else {
                image::ImageReader::open(&path)?
                    .with_guessed_format()?
                    .decode()?
                    .to_rgba8()
            };
            let (w, h) = img.dimensions();
            let tex = create_texture(gl, w, h, img.as_raw(), Filter::Linear, true)?;
            self.sprite_tex = Some((tex, w, h));
        }
        Ok(())
    }

    pub fn uses_audio(&self) -> bool {
        self.config.enabled && self.config.audio_reactive
    }

    pub fn target_bounds(
        &self,
        out_w: i32,
        out_h: i32,
        scale_factor: f32,
        active_window: Option<WindowRect>,
    ) -> (f32, f32, f32, f32) {
        let base_h = (360.0 * self.config.scale.clamp(0.2, 4.0) * scale_factor).max(64.0);
        let aspect = if let Some((_, tw, th)) = self.sprite_tex {
            (tw.max(1) as f32) / (th.max(1) as f32)
        } else {
            0.88
        };
        let base_w = (base_h * aspect).max(64.0);
        let ox = self.config.offset[0] * scale_factor;
        let oy = self.config.offset[1] * scale_factor;
        let sw = out_w as f32;
        let sh = out_h as f32;

        let placement = self.config.effective_placement();

        if matches!(placement, Placement::WindowInside | Placement::WindowOutside) {
            if let Some(win) = active_window {
                let wx = win.x * scale_factor;
                let wy = win.y * scale_factor;
                let ww = win.w * scale_factor;
                let wh = win.h * scale_factor;
                if ww >= 80.0 && wh >= 80.0 {
                    let (mut x, mut y) = match placement {
                        Placement::WindowInside => match self.config.corner {
                            Corner::BottomRight => (wx + ww - base_w - ox, wy + wh - base_h - oy),
                            Corner::BottomLeft => (wx + ox, wy + wh - base_h - oy),
                            Corner::TopRight => (wx + ww - base_w - ox, wy + oy),
                            Corner::TopLeft => (wx + ox, wy + oy),
                            Corner::BottomCenter => (wx + (ww - base_w) * 0.5 + ox, wy + wh - base_h - oy),
                        },
                        Placement::WindowOutside => {
                            // Perch on the outside edge/corner of the focused window:
                            // - TopLeft / TopRight: sitting on top of the window's top bar
                            // - BottomLeft / BottomRight: peeking out from the left/right outer border
                            //   near the bottom corner (or sitting on the top bar if flush with screen edge)
                            let top_sit_y = wy - base_h * 0.82 - oy;
                            match self.config.corner {
                                Corner::TopRight => {
                                    let y = if top_sit_y >= -base_h * 0.25 {
                                        top_sit_y
                                    } else {
                                        wy + oy
                                    };
                                    (wx + ww - base_w - ox, y)
                                }
                                Corner::TopLeft => {
                                    let y = if top_sit_y >= -base_h * 0.25 {
                                        top_sit_y
                                    } else {
                                        wy + oy
                                    };
                                    (wx + ox, y)
                                }
                                Corner::BottomRight => {
                                    let right_outside_x = wx + ww - base_w * 0.22 + ox;
                                    if right_outside_x + base_w <= sw + base_w * 0.25 {
                                        (right_outside_x, wy + wh - base_h - oy)
                                    } else if top_sit_y >= -base_h * 0.25 {
                                        (wx + ww - base_w - ox, top_sit_y)
                                    } else {
                                        (wx + ww - base_w - ox, wy + wh - base_h - oy)
                                    }
                                }
                                Corner::BottomLeft => {
                                    let left_outside_x = wx - base_w * 0.78 - ox;
                                    if left_outside_x >= -base_w * 0.25 {
                                        (left_outside_x, wy + wh - base_h - oy)
                                    } else if top_sit_y >= -base_h * 0.25 {
                                        (wx + ox, top_sit_y)
                                    } else {
                                        (wx + ox, wy + wh - base_h - oy)
                                    }
                                }
                                Corner::BottomCenter => {
                                    let y = if top_sit_y >= -base_h * 0.25 {
                                        top_sit_y
                                    } else {
                                        wy + wh - base_h - oy
                                    };
                                    (wx + (ww - base_w) * 0.5 + ox, y)
                                }
                            }
                        }
                        _ => unreachable!(),
                    };
                    x = x.clamp(-base_w * 0.35, sw - base_w * 0.65);
                    y = y.clamp(-base_h * 0.35, sh - base_h * 0.65);
                    return (x, y, base_w, base_h);
                }
            }
        }

        let (x, y) = match self.config.corner {
            Corner::BottomRight => (sw - base_w - ox, sh - base_h - oy),
            Corner::BottomLeft => (ox, sh - base_h - oy),
            Corner::TopRight => (sw - base_w - ox, oy),
            Corner::TopLeft => (ox, oy),
            Corner::BottomCenter => ((sw - base_w) * 0.5 + ox, sh - base_h - oy),
        };
        (x, y, base_w, base_h)
    }

    pub fn render(
        &mut self,
        gl: &glow::Context,
        info: &FrameInfo,
        active_window: Option<WindowRect>,
    ) -> Result<()> {
        if !self.config.enabled || self.config.opacity <= 0.001 {
            return Ok(());
        }

        let w = info.width.max(1) as f32;
        let h = info.height.max(1) as f32;
        let dt = (info.delta as f32).clamp(0.001, 0.1);
        let (tx, ty, pw, ph) = self.target_bounds(info.width, info.height, 1.0, active_window);

        let pos = match &mut self.smooth_pos {
            Some(p) => {
                let pos_lerp = (dt * 14.0).clamp(0.08, 1.0);
                p[0] += (tx - p[0]) * pos_lerp;
                p[1] += (ty - p[1]) * pos_lerp;
                *p
            }
            None => {
                self.smooth_pos = Some([tx, ty]);
                [tx, ty]
            }
        };

        let cx_px = pos[0] + pw * 0.5;
        let cy_px = h - (pos[1] + ph * 0.5);
        let center_clip = [(cx_px / w) * 2.0 - 1.0, (cy_px / h) * 2.0 - 1.0];
        let half_clip = [pw / w, ph / h];

        let target_look = if self.config.look_at_mouse {
            let dx = ((info.mouse[0] - cx_px) / (w * 0.45)).clamp(-1.0, 1.0);
            let dy = (((h - info.mouse[1]) - cy_px) / (h * 0.45)).clamp(-1.0, 1.0);
            [dx, dy]
        } else {
            [0.0, 0.0]
        };

        let lerp = (dt * 10.0).clamp(0.05, 1.0);
        self.smooth_look[0] += (target_look[0] - self.smooth_look[0]) * lerp;
        self.smooth_look[1] += (target_look[1] - self.smooth_look[1]) * lerp;

        let time = self.start.elapsed().as_secs_f32();
        let breath = (time * 2.4).sin();

        let target_bounce = if self.config.audio_reactive {
            (info.audio[0] * 0.035 + info.beat * 0.045) * half_clip[1]
        } else {
            0.0
        };
        self.smooth_bounce += (target_bounce - self.smooth_bounce) * (dt * 18.0).clamp(0.1, 1.0);

        let mode = if self.sprite_tex.is_some() {
            0
        } else {
            match self.config.source.to_ascii_lowercase().as_str() {
                "neko" => 2,
                "cyber" => 3,
                "spirit" => 4,
                _ => 1, // "reze" or "default"
            }
        };

        let audio = if self.config.audio_reactive {
            info.audio
        } else {
            [0.0; 4]
        };
        let beat = if self.config.audio_reactive {
            info.beat
        } else {
            0.0
        };
        let mouth = if self.config.audio_reactive {
            info.mouth
        } else {
            [0.0; 4]
        };

        unsafe {
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

            self.program.bind(gl);
            self.program
                .set_vec2(gl, "u_center", center_clip[0], center_clip[1]);
            self.program
                .set_vec2(gl, "u_half_size", half_clip[0], half_clip[1]);
            self.program
                .set_vec2(gl, "u_tilt", self.smooth_look[0], self.smooth_look[1]);
            self.program.set_f32(gl, "u_breath", breath);
            self.program.set_f32(gl, "u_bounce", self.smooth_bounce);
            self.program
                .set_i32(gl, "u_flip_x", i32::from(self.config.flip_x));
            self.program.set_i32(gl, "u_mode", mode);
            self.program.set_f32(gl, "u_time", time);
            self.program.set_f32(gl, "u_opacity", self.config.opacity);
            self.program
                .set_vec2(gl, "u_look", self.smooth_look[0], self.smooth_look[1]);
            self.program.set_vec4(gl, "u_audio", audio);
            self.program.set_f32(gl, "u_beat", beat);
            self.program.set_vec4(gl, "u_mouth", mouth);

            gl.active_texture(glow::TEXTURE0);
            let tex = self
                .sprite_tex
                .map(|(t, _, _)| t)
                .unwrap_or(self.dummy_tex);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.program.set_i32(gl, "u_tex", 0);

            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.disable(glow::BLEND);
        }

        Ok(())
    }
}

fn create_quad(gl: &glow::Context) -> Result<(glow::VertexArray, glow::Buffer)> {
    const VERTS: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
    unsafe {
        let vao = gl.create_vertex_array().map_err(|e| anyhow::anyhow!(e))?;
        let vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            super::gl::bytemuck_cast(&VERTS),
            glow::STATIC_DRAW,
        );
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.bind_vertex_array(None);
        Ok((vao, vbo))
    }
}
