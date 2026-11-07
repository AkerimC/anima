//! Static image, `.tex` texture, and animated GIF wallpapers.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use glow::HasContext;
use image::{AnimationDecoder, Frame, RgbaImage, codecs::gif::GifDecoder};

use crate::pkg::decode_tex;
use super::gl::{Filter, Program, Triangle, cover_uv, create_texture};
use super::{FrameInfo, Renderer};

const VERTEX: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
out vec2 v_uv;
void main() {
    vec2 uv = a_pos * 0.5 + 0.5;
    v_uv = vec2(uv.x, 1.0 - uv.y);
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
uniform vec2 u_uv_scale;
uniform vec3 u_bcs; // brightness, contrast, saturation
out vec4 fragColor;
void main() {
    vec2 uv = (v_uv - 0.5) * u_uv_scale + 0.5;
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
        fragColor = vec4(0.0, 0.0, 0.0, 1.0);
        return;
    }
    vec4 col = texture(u_tex, uv);
    // Brightness & Contrast
    vec3 rgb = (col.rgb - 0.5) * u_bcs.y + 0.5 + u_bcs.x;
    // Saturation
    float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3(luma), rgb, u_bcs.z);
    fragColor = vec4(clamp(rgb, 0.0, 1.0), col.a);
}
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Cover,
    Contain,
    Stretch,
}

impl FitMode {
    pub fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "contain" | "fit" => FitMode::Contain,
            "stretch" | "fill" => FitMode::Stretch,
            _ => FitMode::Cover,
        }
    }
}

pub struct ImageRenderer {
    triangle: Triangle,
    program: Program,
    texture: glow::Texture,
    tex_size: (u32, u32),
    uv_scale: (f32, f32),
    fit: FitMode,
    bcs: [f32; 3],
    width: i32,
    height: i32,
    path: PathBuf,
    gif: Option<GifPlayer>,
    paused: bool,
    last_tick: Instant,
}

impl ImageRenderer {
    pub fn new(gl: &glow::Context, path: &Path, width: i32, height: i32) -> Result<Self> {
        let triangle = Triangle::new(gl)?;
        let program = Program::new(gl, VERTEX, FRAGMENT)?;
        let (texture, tex_size, gif) = if is_gif(path) {
            let mut gif = GifPlayer::open(path)?;
            let frame = gif.advance(Instant::now());
            let img = frame.unwrap_or_else(|| gif.canvas.clone());
            let tex = upload(gl, &img)?;
            (tex, img.dimensions(), Some(gif))
        } else {
            let img = load_image(path)?;
            let tex = upload(gl, &img)?;
            (tex, img.dimensions(), None)
        };

        let mut renderer = Self {
            triangle,
            program,
            texture,
            tex_size,
            uv_scale: (1.0, 1.0),
            fit: FitMode::Cover,
            bcs: [0.0, 1.0, 1.0],
            width,
            height,
            path: path.to_path_buf(),
            gif,
            paused: false,
            last_tick: Instant::now(),
        };
        renderer.update_uv();
        Ok(renderer)
    }

    fn update_uv(&mut self) {
        let (sw, sh) = (self.tex_size.0 as f32, self.tex_size.1 as f32);
        let (dw, dh) = (self.width as f32, self.height as f32);
        self.uv_scale = match self.fit {
            FitMode::Cover => cover_uv(sw, sh, dw, dh),
            FitMode::Stretch => (1.0, 1.0),
            FitMode::Contain => {
                let (cx, cy) = cover_uv(sw, sh, dw, dh);
                (1.0 / cx.max(0.001), 1.0 / cy.max(0.001))
            }
        };
    }
}

impl Renderer for ImageRenderer {
    fn render(&mut self, gl: &glow::Context, _frame: &FrameInfo) -> Result<bool> {
        if let Some(gif) = &mut self.gif {
            let now = Instant::now();
            if !self.paused {
                if let Some(img) = gif.advance(now) {
                    unsafe {
                        gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
                        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                        gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            0,
                            0,
                            img.width() as i32,
                            img.height() as i32,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(img.as_raw())),
                        );
                        gl.bind_texture(glow::TEXTURE_2D, None);
                    }
                    let dims = img.dimensions();
                    if dims != self.tex_size {
                        self.tex_size = dims;
                        self.update_uv();
                    }
                }
            }
            self.last_tick = now;
        }

        unsafe {
            gl.viewport(0, 0, self.width, self.height);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            self.program.bind(gl);
            self.program
                .set_vec2(gl, "u_uv_scale", self.uv_scale.0, self.uv_scale.1);
            self.program
                .set_vec3(gl, "u_bcs", self.bcs[0], self.bcs[1], self.bcs[2]);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            self.program.set_i32(gl, "u_tex", 0);
            self.triangle.draw(gl);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }

        Ok(self.gif.is_some())
    }

    fn resize(&mut self, _gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        self.width = width;
        self.height = height;
        self.update_uv();
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        if !paused && self.paused {
            if let Some(gif) = &mut self.gif {
                gif.reset_clock();
            }
        }
        self.paused = paused;
    }

    fn set_volume(&mut self, _volume: f32) {}

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        match key.to_ascii_lowercase().as_str() {
            "fit" | "scaling" => {
                self.fit = FitMode::from_str(value);
                self.update_uv();
            }
            "brightness" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs[0] = v.clamp(-1.0, 1.0);
                }
            }
            "contrast" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs[1] = v.clamp(0.0, 3.0);
                }
            }
            "saturation" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs[2] = v.clamp(0.0, 3.0);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("image {}", self.path.display())
    }

    fn is_animated(&self) -> bool {
        self.gif.is_some()
    }

    fn destroy(&mut self, gl: &glow::Context) {
        unsafe {
            gl.delete_texture(self.texture);
            gl.delete_vertex_array(self.triangle.vao);
            gl.delete_buffer(self.triangle.vbo);
        }
        self.program.destroy(gl);
    }
}

fn is_gif(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gif"))
        .unwrap_or(false)
}

fn upload(gl: &glow::Context, img: &RgbaImage) -> Result<glow::Texture> {
    create_texture(
        gl,
        img.width(),
        img.height(),
        img.as_raw(),
        Filter::Linear,
        true,
    )
}

fn load_image(path: &Path) -> Result<RgbaImage> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "tex" {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        return Ok(decode_tex(&bytes)?.image);
    }
    let img = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()
        .with_context(|| format!("detecting format of {}", path.display()))?
        .decode()
        .with_context(|| format!("decoding {}", path.display()))?;
    Ok(img.to_rgba8())
}

struct GifPlayer {
    path: PathBuf,
    frames: Option<image::Frames<'static>>,
    canvas: RgbaImage,
    delay: Duration,
    next_at: Instant,
}

impl GifPlayer {
    fn open(path: &Path) -> Result<Self> {
        let (frames, canvas) = open_decoder(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            frames: Some(frames),
            canvas,
            delay: Duration::from_millis(100),
            next_at: Instant::now(),
        })
    }

    fn reset_clock(&mut self) {
        self.next_at = Instant::now();
    }

    fn advance(&mut self, now: Instant) -> Option<RgbaImage> {
        if now < self.next_at {
            return None;
        }

        let frame = match self.frames.as_mut().and_then(Iterator::next) {
            Some(Ok(frame)) => frame,
            Some(Err(err)) => {
                log::warn!("gif decode error in {}: {err}", self.path.display());
                self.restart();
                return None;
            }
            None => {
                self.restart();
                return self.advance(now);
            }
        };

        let delay = frame_delay(&frame);
        composite(&mut self.canvas, &frame);
        self.delay = delay;
        self.next_at = now + delay;
        Some(self.canvas.clone())
    }

    fn restart(&mut self) {
        match open_decoder(&self.path) {
            Ok((frames, canvas)) => {
                self.frames = Some(frames);
                self.canvas = canvas;
                self.next_at = Instant::now();
            }
            Err(err) => {
                log::warn!("cannot loop gif {}: {err}", self.path.display());
                self.frames = None;
            }
        }
    }
}

fn open_decoder(path: &Path) -> Result<(image::Frames<'static>, RgbaImage)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let decoder = GifDecoder::new(BufReader::new(file))
        .with_context(|| format!("reading gif {}", path.display()))?;
    let (w, h) = image::ImageDecoder::dimensions(&decoder);
    Ok((
        decoder.into_frames(),
        RgbaImage::from_pixel(w.max(1), h.max(1), image::Rgba([0, 0, 0, 0])),
    ))
}

fn frame_delay(frame: &Frame) -> Duration {
    let (num, den) = frame.delay().numer_denom_ms();
    let ms = if den == 0 { 100 } else { num / den };
    Duration::from_millis(ms.clamp(10, 10_000) as u64)
}

fn composite(canvas: &mut RgbaImage, frame: &Frame) {
    let sub = frame.buffer();
    let (left, top) = (frame.left(), frame.top());
    for (x, y, px) in sub.enumerate_pixels() {
        let (cx, cy) = (left + x, top + y);
        if cx >= canvas.width() || cy >= canvas.height() {
            continue;
        }
        let src = px.0;
        if src[3] == 0 {
            continue;
        }
        let dst = canvas.get_pixel_mut(cx, cy);
        let a = src[3] as f32 / 255.0;
        for i in 0..3 {
            dst.0[i] = (src[i] as f32 * a + dst.0[i] as f32 * (1.0 - a)).round() as u8;
        }
        dst.0[3] = 255;
    }
}
