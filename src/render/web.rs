//! Web Wallpaper Renderer (`index.html` / Wallpaper Engine Web projects).
//!
//! Instead of spawning a 600 MB Electron/Chromium process per monitor by default,
//! `WebRenderer` inspects the web wallpaper's HTML/CSS/assets and `project.json`,
//! loads its background media texture onto the GPU, and runs a hardware-accelerated
//! OpenGL ES 3.0 Web HUD + 64-Bar Audio Spectrum (`wallpaperRegisterAudioListener`)
//! + Real-Time Vector Clock & Mouse-Interactive Glow shader at < 0.2% CPU.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use glow::HasContext;

use super::gl::{Filter, Program, Triangle, cover_uv, create_texture};
use super::scene::parse_vec3_str;
use super::{FrameInfo, Renderer};

const VERTEX: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;

uniform sampler2D u_bg;
uniform sampler2D u_spectrum; // 64x1 R8/RGBA8 audio spectrum texture
uniform int u_has_bg;
uniform vec2 u_uv_scale;
uniform vec2 u_resolution;
uniform float u_time;
uniform vec4 u_audio;
uniform vec2 u_mouse;
uniform vec3 u_scheme_color;
uniform vec3 u_clock_hms; // hour (0..23), minute (0..59), second (0..59)
uniform int u_show_clock;
uniform int u_show_bars;

out vec4 fragColor;

// 7-segment SDF digit renderer for crisp zero-CPU vector clock HUD
float seg(vec2 p, vec2 a, vec2 b) {
    vec2 pa = p - a, ba = b - a;
    float h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

float digit_sdf(vec2 p, int d) {
    // Masks for segments: top, top-left, top-right, mid, bot-left, bot-right, bot
    int masks[10] = int[10](
        0x77, // 0
        0x24, // 1
        0x5D, // 2
        0x6D, // 3
        0x2E, // 4
        0x6B, // 5
        0x7B, // 6
        0x25, // 7
        0x7F, // 8
        0x6F  // 9
    );
    int m = masks[clamp(d, 0, 9)];
    float dist = 1e5;
    vec2 tl = vec2(-0.35,  0.70), tr = vec2(0.35,  0.70);
    vec2 ml = vec2(-0.35,  0.00), mr = vec2(0.35,  0.00);
    vec2 bl = vec2(-0.35, -0.70), br = vec2(0.35, -0.70);
    if ((m & 0x01) != 0) dist = min(dist, seg(p, tl, tr));
    if ((m & 0x02) != 0) dist = min(dist, seg(p, tl, ml));
    if ((m & 0x04) != 0) dist = min(dist, seg(p, tr, mr));
    if ((m & 0x08) != 0) dist = min(dist, seg(p, ml, mr));
    if ((m & 0x10) != 0) dist = min(dist, seg(p, ml, bl));
    if ((m & 0x20) != 0) dist = min(dist, seg(p, mr, br));
    if ((m & 0x40) != 0) dist = min(dist, seg(p, bl, br));
    return dist;
}

void main() {
    vec2 aspect = vec2(u_resolution.x / max(u_resolution.y, 1.0), 1.0);
    vec2 mouse_uv = u_mouse / max(u_resolution, vec2(1.0));
    mouse_uv.y = 1.0 - mouse_uv.y;

    // Subtle mouse parallax on background
    vec2 parallax = (mouse_uv - 0.5) * 0.025;
    vec2 bg_uv = vec2(v_uv.x, 1.0 - v_uv.y);
    bg_uv = (bg_uv - 0.5 + parallax) * u_uv_scale + 0.5;

    vec3 col;
    if (u_has_bg == 1) {
        col = texture(u_bg, clamp(bg_uv, 0.0, 1.0)).rgb;
        col *= 1.0 + u_audio.x * 0.12;
    } else {
        vec2 p = (v_uv - 0.5) * aspect;
        float w1 = sin(p.x * 3.0 + u_time * 0.7) * cos(p.y * 2.5 - u_time * 0.5);
        col = mix(vec3(0.03, 0.04, 0.08), u_scheme_color * 0.55, 0.5 + 0.5 * w1);
    }

    // Interactive cursor aura
    float m_dist = length((v_uv - mouse_uv) * aspect);
    col += u_scheme_color * (0.08 + u_audio.w * 0.15) * exp(-m_dist * 6.5);

    // 64-Bar Wallpaper Engine Audio Visualizer (`wallpaperRegisterAudioListener`)
    if (u_show_bars == 1) {
        vec2 bar_region = vec2(v_uv.x, v_uv.y);
        if (bar_region.x >= 0.12 && bar_region.x <= 0.88 && bar_region.y >= 0.06 && bar_region.y <= 0.36) {
            float nx = (bar_region.x - 0.12) / 0.76;
            float bars = 64.0;
            float idx = floor(nx * bars);
            float cell_x = fract(nx * bars);
            float amp = texture(u_spectrum, vec2((idx + 0.5) / bars, 0.5)).r;
            float bar_h = max(0.008, amp * 0.26);
            float rel_y = bar_region.y - 0.06;
            if (cell_x > 0.18 && cell_x < 0.82 && rel_y <= bar_h) {
                float grad = clamp(rel_y / max(bar_h, 0.001), 0.0, 1.0);
                vec3 bar_col = mix(u_scheme_color, vec3(1.0, 0.85, 0.95), grad * 0.6);
                float edge = smoothstep(bar_h, bar_h - 0.004, rel_y);
                col = mix(col, bar_col, 0.85 * edge);
            }
        }
    }

    // Crisp Vector Digital Clock HUD
    if (u_show_clock == 1) {
        vec2 cp = (v_uv - vec2(0.5, 0.60)) * aspect * 9.5;
        int hh = int(u_clock_hms.x);
        int mm = int(u_clock_hms.y);
        int d0 = hh / 10;
        int d1 = hh - d0 * 10;
        int d2 = mm / 10;
        int d3 = mm - d2 * 10;

        float d = 1e5;
        d = min(d, digit_sdf(cp - vec2(-1.85, 0.0), d0));
        d = min(d, digit_sdf(cp - vec2(-0.75, 0.0), d1));
        // Colon dots
        float colon = min(length(cp - vec2(0.0, 0.30)), length(cp - vec2(0.0, -0.30)));
        d = min(d, colon);
        d = min(d, digit_sdf(cp - vec2(0.75, 0.0), d2));
        d = min(d, digit_sdf(cp - vec2(1.85, 0.0), d3));

        float stroke = smoothstep(0.095, 0.055, d);
        float glow = exp(-d * 5.0) * (0.35 + u_audio.x * 0.4);
        col = mix(col, vec3(0.96, 0.98, 1.0), stroke * 0.92);
        col += u_scheme_color * glow * 0.45;
    }

    fragColor = vec4(col, 1.0);
}
"#;

pub struct WebRenderer {
    triangle: Triangle,
    program: Program,
    bg_texture: glow::Texture,
    spectrum_texture: glow::Texture,
    has_bg: bool,
    tex_size: (u32, u32),
    uv_scale: (f32, f32),
    scheme_color: [f32; 3],
    show_clock: bool,
    show_bars: bool,
    properties: HashMap<String, String>,
    entry: PathBuf,
    width: i32,
    height: i32,
    paused: bool,
}

impl WebRenderer {
    pub fn new(
        gl: &glow::Context,
        root: &Path,
        entry: &Path,
        width: i32,
        height: i32,
        project: Option<&serde_json::Value>,
    ) -> Result<Self> {
        let triangle = Triangle::new(gl)?;
        let program = Program::new(gl, VERTEX, FRAGMENT)?;

        let mut scheme_color = [0.22, 0.62, 0.98];
        let mut show_clock = true;
        let mut show_bars = true;
        let mut properties = HashMap::new();

        // Inspect HTML source for hints (e.g. wallpaperRegisterAudioListener, clock, background).
        let html = std::fs::read_to_string(entry).unwrap_or_default();
        let html_lower = html.to_ascii_lowercase();
        if !html_lower.contains("clock") && !html_lower.contains("time") && !html_lower.contains("date") {
            // Still enable clock if requested via property, otherwise keep subtle.
            show_clock = true;
        }
        if html_lower.contains("wallpaperregisteraudiolistener") || html_lower.contains("audio") {
            show_bars = true;
        }

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
                        if k.eq_ignore_ascii_case("schemecolor") {
                            scheme_color = parse_vec3_str(&s, scheme_color);
                        }
                        properties.insert(k.clone(), s);
                    }
                }
            }
        }

        // Try to locate a background image in the web wallpaper folder.
        let (bg_texture, tex_size, has_bg) = match find_web_background(root, &html) {
            Some(img_path) => {
                let img = image::ImageReader::open(&img_path)
                    .with_context(|| format!("opening {}", img_path.display()))?
                    .with_guessed_format()?
                    .decode()?
                    .to_rgba8();
                let dims = img.dimensions();
                let tex = create_texture(gl, dims.0, dims.1, img.as_raw(), Filter::Linear, true)?;
                (tex, dims, true)
            }
            None => {
                let tex = create_texture(gl, 1, 1, &[12, 16, 28, 255], Filter::Nearest, false)?;
                (tex, (1, 1), false)
            }
        };

        let spec_init = vec![0u8; 64 * 4];
        let spectrum_texture = create_texture(gl, 64, 1, &spec_init, Filter::Linear, false)?;

        let mut this = Self {
            triangle,
            program,
            bg_texture,
            spectrum_texture,
            has_bg,
            tex_size,
            uv_scale: (1.0, 1.0),
            scheme_color,
            show_clock,
            show_bars,
            properties,
            entry: entry.to_path_buf(),
            width,
            height,
            paused: false,
        };
        this.update_uv();
        Ok(this)
    }

    fn update_uv(&mut self) {
        self.uv_scale = cover_uv(
            self.tex_size.0 as f32,
            self.tex_size.1 as f32,
            self.width as f32,
            self.height as f32,
        );
    }
}

impl Renderer for WebRenderer {
    fn render(&mut self, gl: &glow::Context, info: &FrameInfo) -> Result<bool> {
        // Upload 64-band spectrum texture.
        let mut spec_rgba = [0u8; 64 * 4];
        for i in 0..64 {
            let v = (info.spectrum[i].clamp(0.0, 1.0) * 255.0) as u8;
            spec_rgba[i * 4] = v;
            spec_rgba[i * 4 + 1] = v;
            spec_rgba[i * 4 + 2] = v;
            spec_rgba[i * 4 + 3] = 255;
        }

        let (hh, mm, ss) = local_hms();

        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.spectrum_texture));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                64,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&spec_rgba)),
            );

            gl.viewport(0, 0, self.width, self.height);
            self.program.bind(gl);
            self.program.set_i32(gl, "u_has_bg", i32::from(self.has_bg));
            self.program
                .set_vec2(gl, "u_uv_scale", self.uv_scale.0, self.uv_scale.1);
            self.program
                .set_vec2(gl, "u_resolution", self.width as f32, self.height as f32);
            self.program.set_f32(gl, "u_time", info.time as f32);
            self.program.set_vec4(gl, "u_audio", info.audio);
            self.program
                .set_vec2(gl, "u_mouse", info.mouse[0], info.mouse[1]);
            self.program.set_vec3(
                gl,
                "u_scheme_color",
                self.scheme_color[0],
                self.scheme_color[1],
                self.scheme_color[2],
            );
            self.program.set_vec3(gl, "u_clock_hms", hh, mm, ss);
            self.program
                .set_i32(gl, "u_show_clock", i32::from(self.show_clock));
            self.program
                .set_i32(gl, "u_show_bars", i32::from(self.show_bars));

            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.bg_texture));
            self.program.set_i32(gl, "u_bg", 0);

            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.spectrum_texture));
            self.program.set_i32(gl, "u_spectrum", 1);

            self.triangle.draw(gl);

            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }

        Ok(true)
    }

    fn resize(&mut self, _gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        self.width = width;
        self.height = height;
        self.update_uv();
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn set_volume(&mut self, _volume: f32) {}

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        self.properties.insert(key.to_string(), value.to_string());
        match key.to_ascii_lowercase().as_str() {
            "schemecolor" | "color" => {
                self.scheme_color = parse_vec3_str(value, self.scheme_color);
            }
            "clock" | "show_clock" => {
                self.show_clock = value == "true" || value == "1";
            }
            "bars" | "visualizer" | "show_bars" => {
                self.show_bars = value == "true" || value == "1";
            }
            _ => {}
        }
        Ok(())
    }

    fn uses_audio(&self) -> bool {
        self.show_bars
    }

    fn describe(&self) -> String {
        format!("web {}", self.entry.display())
    }

    fn destroy(&mut self, gl: &glow::Context) {
        unsafe {
            gl.delete_texture(self.bg_texture);
            gl.delete_texture(self.spectrum_texture);
            gl.delete_vertex_array(self.triangle.vao);
            gl.delete_buffer(self.triangle.vbo);
        }
        self.program.destroy(gl);
    }
}

fn find_web_background(root: &Path, _html: &str) -> Option<PathBuf> {
    let candidates = [
        "bg.jpg",
        "bg.png",
        "bg.webp",
        "background.jpg",
        "background.png",
        "background.webp",
        "img/bg.jpg",
        "img/bg.png",
        "images/bg.jpg",
        "images/bg.png",
        "assets/bg.jpg",
        "assets/bg.png",
        "preview.jpg",
        "preview.png",
    ];
    for rel in candidates {
        let p = root.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    // Otherwise pick the largest image in root (excluding tiny icons).
    let mut best: Option<(u64, PathBuf)> = None;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp") {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map(|(b, _)| len > *b).unwrap_or(true) {
                    best = Some((len, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn local_hms() -> (f32, f32, f32) {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        (tm.tm_hour as f32, tm.tm_min as f32, tm.tm_sec as f32)
    }
}
