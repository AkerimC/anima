//! Headless offscreen EGL regression runner for Scene, Web, Video, Image, and Shader wallpapers.

use std::io::Write;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use glow::HasContext;
use glutin::config::{ConfigSurfaceTypes, ConfigTemplateBuilder};
use glutin::context::{ContextApi, ContextAttributesBuilder, PossiblyCurrentContext, Version};
use glutin::display::GlDisplay;
use glutin::prelude::*;
use glutin::surface::{PbufferSurface, Surface, SurfaceAttributesBuilder};
use serde::Serialize;

use crate::render::image::ImageRenderer;
use crate::render::scene::{SceneDiagnostics, SceneRenderer};
use crate::render::shader::ShaderRenderer;
use crate::render::video::VideoRenderer;
use crate::render::web::{WebOptions, WebRenderer};
use crate::render::{FrameInfo, Renderer};
use crate::wallpaper::{self, Wallpaper, WallpaperKind};

/// Options controlling headless wallpaper verification.
#[derive(Debug, Clone)]
pub struct VerifyOptions {
    pub kind: Option<WallpaperKind>,
    pub filter: Option<String>,
    pub dir: Option<PathBuf>,
    pub width: i32,
    pub height: i32,
    pub scene_frames: usize,
    pub web_timeout_ms: u64,
    pub video_timeout_ms: u64,
    pub hwdec: String,
    pub dump_dir: Option<PathBuf>,
    pub rss_limit_mb: Option<f64>,
    pub json: bool,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            kind: None,
            filter: None,
            dir: None,
            width: 640,
            height: 360,
            scene_frames: 4,
            web_timeout_ms: 6000,
            video_timeout_ms: 4000,
            hwdec: "auto-safe".to_string(),
            dump_dir: None,
            rss_limit_mb: None,
            json: false,
        }
    }
}

/// Overall verification outcome for a single wallpaper item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyStatus {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl VerifyStatus {
    fn tag(self) -> &'static str {
        match self {
            VerifyStatus::Ok => "OK  ",
            VerifyStatus::Warn => "WARN",
            VerifyStatus::Fail => "FAIL",
            VerifyStatus::Skip => "SKIP",
        }
    }
}

/// Pixel statistics computed from the final readback frame.
#[derive(Debug, Clone, Default, Serialize)]
pub struct FrameStats {
    pub mean_luma: f32,
    pub max_luma: f32,
    pub luma_std: f32,
    pub non_black_ratio: f32,
    pub transparent_ratio: f32,
}

/// Telemetry collected while verifying a single wallpaper.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyItemReport {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub path: String,
    pub status: VerifyStatus,
    pub load_ms: u64,
    pub render_ms: u64,
    pub rss_mb: f64,
    pub hwm_mb: f64,
    pub gl_errors: Vec<u32>,
    pub frame: Option<FrameStats>,
    pub scene: Option<SceneDiagnostics>,
    pub web_live: Option<bool>,
    pub web_alive: Option<bool>,
    pub video_frames: Option<u32>,
    pub video_codec: Option<String>,
    pub hwdec: Option<String>,
    pub notes: Vec<String>,
    pub error: Option<String>,
}

/// Aggregate report returned by [`run_verify`].
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub gl_renderer: String,
    pub gl_version: String,
    pub total: usize,
    pub passed: usize,
    pub warned: usize,
    pub failed: usize,
    pub skipped: usize,
    pub elapsed_ms: u64,
    pub peak_rss_mb: f64,
    pub peak_hwm_mb: f64,
    pub total_shader_failures: usize,
    pub total_script_failures: usize,
    pub total_gl_errors: usize,
    pub items: Vec<VerifyItemReport>,
}

impl VerifyReport {
    fn from_items(
        gl_renderer: String,
        gl_version: String,
        elapsed_ms: u64,
        items: Vec<VerifyItemReport>,
    ) -> Self {
        let mut passed = 0usize;
        let mut warned = 0usize;
        let mut failed = 0usize;
        let mut skipped = 0usize;
        let mut peak_rss_mb = 0.0f64;
        let mut peak_hwm_mb = 0.0f64;
        let mut total_shader_failures = 0usize;
        let mut total_script_failures = 0usize;
        let mut total_gl_errors = 0usize;
        for item in &items {
            match item.status {
                VerifyStatus::Ok => passed += 1,
                VerifyStatus::Warn => warned += 1,
                VerifyStatus::Fail => failed += 1,
                VerifyStatus::Skip => skipped += 1,
            }
            peak_rss_mb = peak_rss_mb.max(item.rss_mb);
            peak_hwm_mb = peak_hwm_mb.max(item.hwm_mb);
            total_gl_errors += item.gl_errors.len();
            if let Some(scene) = &item.scene {
                total_shader_failures += scene.shader_failures.len();
                total_script_failures += scene.script_failures;
            }
        }
        Self {
            gl_renderer,
            gl_version,
            total: items.len(),
            passed,
            warned,
            failed,
            skipped,
            elapsed_ms,
            peak_rss_mb,
            peak_hwm_mb,
            total_shader_failures,
            total_script_failures,
            total_gl_errors,
            items,
        }
    }
}

/// Offscreen EGL Pbuffer OpenGL ES 3.0 context.
///
/// Field declaration order ensures context and surface drop before the EGL display.
struct HeadlessGl {
    gl: glow::Context,
    _ctx: PossiblyCurrentContext,
    _surface: Surface<PbufferSurface>,
    _display: glutin::api::egl::display::Display,
    width: i32,
    height: i32,
    renderer_name: String,
    gl_version: String,
}

impl HeadlessGl {
    fn new(width: i32, height: i32) -> Result<Self> {
        let w = NonZeroU32::new(width.max(1) as u32).ok_or_else(|| anyhow!("invalid width"))?;
        let h = NonZeroU32::new(height.max(1) as u32).ok_or_else(|| anyhow!("invalid height"))?;
        let devices = glutin::api::egl::device::Device::query_devices()
            .context("querying EGL devices")?;
        let mut errors = Vec::new();
        for (idx, device) in devices.enumerate() {
            let display = match unsafe { glutin::api::egl::display::Display::with_device(&device, None) } {
                Ok(d) => d,
                Err(err) => {
                    errors.push(format!("device {idx} display: {err}"));
                    continue;
                }
            };
            let primary_tmpl = ConfigTemplateBuilder::new()
                .with_alpha_size(8)
                .with_surface_type(ConfigSurfaceTypes::PBUFFER)
                .build();
            let fallback_tmpl = ConfigTemplateBuilder::new()
                .with_surface_type(ConfigSurfaceTypes::PBUFFER)
                .build();
            let config = unsafe { display.find_configs(primary_tmpl) }
                .ok()
                .and_then(|mut it| it.next())
                .or_else(|| {
                    unsafe { display.find_configs(fallback_tmpl) }
                        .ok()
                        .and_then(|mut it| it.next())
                });
            let Some(config) = config else {
                errors.push(format!("device {idx}: no PBUFFER config"));
                continue;
            };
            let surf_attrs = SurfaceAttributesBuilder::<PbufferSurface>::new().build(w, h);
            let surface = match unsafe { display.create_pbuffer_surface(&config, &surf_attrs) } {
                Ok(s) => s,
                Err(err) => {
                    errors.push(format!("device {idx} pbuffer: {err}"));
                    continue;
                }
            };
            let ctx_attrs = ContextAttributesBuilder::new()
                .with_context_api(ContextApi::Gles(Some(Version::new(3, 0))))
                .build(None);
            let not_current = match unsafe { display.create_context(&config, &ctx_attrs) } {
                Ok(c) => c,
                Err(err) => {
                    errors.push(format!("device {idx} gles3 context: {err}"));
                    continue;
                }
            };
            let ctx = match not_current.make_current(&surface) {
                Ok(c) => c,
                Err(err) => {
                    errors.push(format!("device {idx} make_current: {err}"));
                    continue;
                }
            };
            let gl = unsafe {
                glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s) as *const _)
            };
            let renderer_name = unsafe { gl.get_parameter_string(glow::RENDERER) };
            let gl_version = unsafe { gl.get_parameter_string(glow::VERSION) };
            return Ok(Self {
                gl,
                _ctx: PossiblyCurrentContext::Egl(ctx),
                _surface: Surface::Egl(surface),
                _display: display,
                width: w.get() as i32,
                height: h.get() as i32,
                renderer_name,
                gl_version,
            });
        }
        Err(anyhow!(
            "failed to create offscreen EGL ES 3.0 Pbuffer context ({})",
            errors.join("; ")
        ))
    }

    fn drain_gl_errors(&self) -> Vec<u32> {
        let mut errs = Vec::new();
        for _ in 0..32 {
            let e = unsafe { self.gl.get_error() };
            if e == glow::NO_ERROR {
                break;
            }
            errs.push(e);
        }
        errs
    }

    fn reset_framebuffer(&self) {
        let _ = self.drain_gl_errors();
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.viewport(0, 0, self.width, self.height);
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::BLEND);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    fn read_rgba(&self) -> Vec<u8> {
        let w = self.width.max(1) as usize;
        let h = self.height.max(1) as usize;
        let mut rgba = vec![0u8; w * h * 4];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.finish();
            self.gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            self.gl.read_pixels(
                0,
                0,
                self.width,
                self.height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut rgba)),
            );
        }
        // Flip OpenGL bottom-up rows to top-down image layout.
        let row_bytes = w * 4;
        for top in 0..(h / 2) {
            let bot = h - 1 - top;
            let (lo, hi) = rgba.split_at_mut(bot * row_bytes);
            lo[top * row_bytes..(top + 1) * row_bytes].swap_with_slice(&mut hi[..row_bytes]);
        }
        rgba
    }
}

/// Compute mean/max/std luminance and non-black/transparent coverage over RGBA8 pixels.
pub fn compute_frame_stats(rgba: &[u8]) -> FrameStats {
    let pixel_count = rgba.len() / 4;
    if pixel_count == 0 {
        return FrameStats::default();
    }
    let mut sum = 0.0f64;
    let mut sum_sq = 0.0f64;
    let mut max_luma = 0.0f32;
    let mut non_black = 0usize;
    let mut transparent = 0usize;
    for px in rgba.chunks_exact(4) {
        let r = px[0] as f32 / 255.0;
        let g = px[1] as f32 / 255.0;
        let b = px[2] as f32 / 255.0;
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        sum += luma as f64;
        sum_sq += (luma as f64) * (luma as f64);
        if luma > max_luma {
            max_luma = luma;
        }
        if px[0].max(px[1]).max(px[2]) > 4 {
            non_black += 1;
        }
        if px[3] == 0 {
            transparent += 1;
        }
    }
    let n = pixel_count as f64;
    let mean = sum / n;
    let var = (sum_sq / n - mean * mean).max(0.0);
    FrameStats {
        mean_luma: mean as f32,
        max_luma,
        luma_std: var.sqrt() as f32,
        non_black_ratio: (non_black as f64 / n) as f32,
        transparent_ratio: (transparent as f64 / n) as f32,
    }
}

/// Parse `(VmRSS_mb, VmHWM_mb)` from Linux `/proc/self/status` contents.
pub fn parse_proc_status_rss_mb(status_text: &str) -> (f64, f64) {
    let parse_kb = |prefix: &str| -> f64 {
        status_text
            .lines()
            .find_map(|line| {
                let rest = line.strip_prefix(prefix)?.trim();
                let kb_str = rest.split_whitespace().next()?;
                kb_str.parse::<f64>().ok()
            })
            .map(|kb| kb / 1024.0)
            .unwrap_or(0.0)
    };
    (parse_kb("VmRSS:"), parse_kb("VmHWM:"))
}

fn read_proc_rss_mb() -> (f64, f64) {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return (0.0, 0.0);
    };
    parse_proc_status_rss_mb(&text)
}

#[cfg(target_env = "gnu")]
unsafe extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

fn trim_allocator() {
    #[cfg(target_env = "gnu")]
    unsafe {
        let _ = malloc_trim(0);
    }
}

fn synthetic_frame(width: i32, height: i32, frame_idx: u64, time: f64) -> FrameInfo {
    let mut spectrum = [0.0f32; 64];
    for (i, slot) in spectrum.iter_mut().enumerate() {
        let phase = (i as f32) * 0.25 + (time as f32) * 3.0;
        *slot = (0.18 + 0.14 * phase.sin()).clamp(0.02, 1.0);
    }
    let wf = width.max(1) as f32;
    let hf = height.max(1) as f32;
    let mx = wf * (0.25 + 0.5 * ((frame_idx as f32 * 0.2).fract()));
    let my = hf * 0.5;
    FrameInfo {
        width,
        height,
        time,
        delta: 1.0 / 30.0,
        frame: frame_idx,
        paused: false,
        audio: [0.35, 0.28, 0.22, 0.30],
        spectrum,
        beat: if frame_idx.is_multiple_of(2) { 0.45 } else { 0.1 },
        mouth: [0.25, 0.0, 0.1, 0.5],
        mouse: [mx, my],
        mouse_down: false,
        scale: 1.0,
        media: None,
    }
}

fn sanitize_id_filename(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = s.trim_matches('_');
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed.to_string()
    }
}

fn save_dump_png(
    dump_dir: Option<&Path>,
    id: &str,
    width: i32,
    height: i32,
    rgba: Vec<u8>,
    notes: &mut Vec<String>,
) {
    let Some(dir) = dump_dir else {
        return;
    };
    if let Err(err) = std::fs::create_dir_all(dir) {
        notes.push(format!("dump mkdir failed: {err}"));
        return;
    }
    let filename = format!("{}.png", sanitize_id_filename(id));
    let out_path = dir.join(filename);
    if let Some(img) = image::RgbaImage::from_raw(width.max(1) as u32, height.max(1) as u32, rgba) {
        if let Err(err) = img.save(&out_path) {
            notes.push(format!("png save failed: {err}"));
        }
    }
}

fn item_id(wp: &Wallpaper) -> String {
    wp.workshop_id
        .clone()
        .unwrap_or_else(|| sanitize_id_filename(&wp.title))
}

fn kind_rank(kind: WallpaperKind) -> u8 {
    match kind {
        WallpaperKind::Scene => 0,
        WallpaperKind::Web => 1,
        WallpaperKind::Video => 2,
        WallpaperKind::Shader => 3,
        WallpaperKind::Image => 4,
        WallpaperKind::Application => 5,
    }
}

/// Parse a CLI `--kind` string into a [`WallpaperKind`].
pub fn parse_kind(raw: &str) -> Result<WallpaperKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "scene" => Ok(WallpaperKind::Scene),
        "web" => Ok(WallpaperKind::Web),
        "video" => Ok(WallpaperKind::Video),
        "shader" => Ok(WallpaperKind::Shader),
        "image" => Ok(WallpaperKind::Image),
        "application" | "app" => Ok(WallpaperKind::Application),
        other => Err(anyhow!("unknown wallpaper kind '{other}' (expected scene, web, video, shader, image)")),
    }
}

fn check_rss_limit(limit: Option<f64>, rss_mb: f64, notes: &mut Vec<String>) -> bool {
    if let Some(max_mb) = limit {
        if rss_mb > max_mb {
            notes.push(format!("RSS {rss_mb:.1} MiB > limit {max_mb:.1} MiB"));
            return true;
        }
    }
    false
}

fn verify_static_renderer(
    hgl: &HeadlessGl,
    mut renderer: Box<dyn Renderer>,
    opts: &VerifyOptions,
    id: &str,
    load_ms: u64,
    wp: &Wallpaper,
) -> VerifyItemReport {
    let t_render = Instant::now();
    let mut render_err: Option<String> = None;
    let frames = opts.scene_frames.max(1);
    for idx in 0..frames {
        let info = synthetic_frame(hgl.width, hgl.height, idx as u64, idx as f64 * 0.25);
        if let Err(err) = renderer.render(&hgl.gl, &info) {
            render_err = Some(format!("{err:#}"));
            break;
        }
    }
    let gl_errors = hgl.drain_gl_errors();
    let rgba = hgl.read_rgba();
    let stats = compute_frame_stats(&rgba);
    let mut notes = Vec::new();
    save_dump_png(opts.dump_dir.as_deref(), id, hgl.width, hgl.height, rgba, &mut notes);
    let render_ms = t_render.elapsed().as_millis() as u64;
    renderer.destroy(&hgl.gl);
    drop(renderer);
    unsafe { hgl.gl.finish() };
    trim_allocator();
    let (rss_mb, hwm_mb) = read_proc_rss_mb();

    let mut warn = check_rss_limit(opts.rss_limit_mb, rss_mb, &mut notes);
    if stats.non_black_ratio < 0.001 {
        notes.push(format!("near-black frame ({:.4})", stats.non_black_ratio));
        warn = true;
    }
    let status = if render_err.is_some() || !gl_errors.is_empty() {
        VerifyStatus::Fail
    } else if warn {
        VerifyStatus::Warn
    } else {
        VerifyStatus::Ok
    };
    VerifyItemReport {
        id: id.to_string(),
        title: wp.title.clone(),
        kind: wp.kind.as_str().to_string(),
        path: wp.entry.display().to_string(),
        status,
        load_ms,
        render_ms,
        rss_mb,
        hwm_mb,
        gl_errors,
        frame: Some(stats),
        scene: None,
        web_live: None,
        web_alive: None,
        video_frames: None,
        video_codec: None,
        hwdec: None,
        notes,
        error: render_err,
    }
}

fn verify_scene(hgl: &HeadlessGl, wp: &Wallpaper, opts: &VerifyOptions) -> VerifyItemReport {
    let id = item_id(wp);
    let t_load = Instant::now();
    let mut renderer = match SceneRenderer::new(
        &hgl.gl,
        &wp.root,
        &wp.entry,
        hgl.width,
        hgl.height,
        &opts.hwdec,
        wp.project.as_ref(),
    ) {
        Ok(r) => r,
        Err(err) => {
            let gl_errors = hgl.drain_gl_errors();
            trim_allocator();
            let (rss_mb, hwm_mb) = read_proc_rss_mb();
            return VerifyItemReport {
                id,
                title: wp.title.clone(),
                kind: wp.kind.as_str().to_string(),
                path: wp.entry.display().to_string(),
                status: VerifyStatus::Fail,
                load_ms: t_load.elapsed().as_millis() as u64,
                render_ms: 0,
                rss_mb,
                hwm_mb,
                gl_errors,
                frame: None,
                scene: None,
                web_live: None,
                web_alive: None,
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: Vec::new(),
                error: Some(format!("load failed: {err:#}")),
            };
        }
    };
    let load_ms = t_load.elapsed().as_millis() as u64;
    // Never write SceneScript localStorage or output audio during verification.
    renderer.set_persist_storage(false);
    renderer.set_volume(0.0);
    let _ = renderer.set_property("mute", "true");

    let t_render = Instant::now();
    let mut render_err: Option<String> = None;
    let frames = opts.scene_frames.max(1);
    let mut time = 0.0f64;
    for idx in 0..frames {
        time = idx as f64 * 0.25;
        let info = synthetic_frame(hgl.width, hgl.height, idx as u64, time);
        if let Err(err) = renderer.render(&hgl.gl, &info) {
            render_err = Some(format!("render failed: {err:#}"));
            break;
        }
    }
    let mut diag = renderer.diagnostics();
    let mut gl_errors = hgl.drain_gl_errors();
    let mut rgba = hgl.read_rgba();
    let mut stats = compute_frame_stats(&rgba);
    if render_err.is_none() && stats.non_black_ratio < 0.001 {
        for step in 0..12usize {
            let mut info = synthetic_frame(
                hgl.width,
                hgl.height,
                (frames + step) as u64,
                time + (step + 1) as f64 * 0.25,
            );
            info.delta = 0.25;
            if let Err(err) = renderer.render(&hgl.gl, &info) {
                render_err = Some(format!("render failed: {err:#}"));
                break;
            }
            gl_errors.extend(hgl.drain_gl_errors());
            rgba = hgl.read_rgba();
            stats = compute_frame_stats(&rgba);
            if stats.non_black_ratio >= 0.01 {
                break;
            }
        }
        diag = renderer.diagnostics();
    }
    let mut notes = Vec::new();
    save_dump_png(opts.dump_dir.as_deref(), &id, hgl.width, hgl.height, rgba, &mut notes);
    let render_ms = t_render.elapsed().as_millis() as u64;

    renderer.destroy(&hgl.gl);
    drop(renderer);
    unsafe { hgl.gl.finish() };
    trim_allocator();
    let (rss_mb, hwm_mb) = read_proc_rss_mb();

    let mut warn = check_rss_limit(opts.rss_limit_mb, rss_mb, &mut notes);
    if !diag.shader_failures.is_empty() {
        notes.push(format!("{} shader failure(s)", diag.shader_failures.len()));
    }
    if diag.script_failures > 0 {
        notes.push(format!("{} script failure(s)", diag.script_failures));
        warn = true;
    }
    if stats.non_black_ratio < 0.001 {
        notes.push(format!("near-black frame ({:.4})", stats.non_black_ratio));
        warn = true;
    }

    let status = if render_err.is_some() || !gl_errors.is_empty() || !diag.shader_failures.is_empty()
    {
        VerifyStatus::Fail
    } else if warn {
        VerifyStatus::Warn
    } else {
        VerifyStatus::Ok
    };

    VerifyItemReport {
        id,
        title: wp.title.clone(),
        kind: wp.kind.as_str().to_string(),
        path: wp.entry.display().to_string(),
        status,
        load_ms,
        render_ms,
        rss_mb,
        hwm_mb,
        gl_errors,
        frame: Some(stats),
        scene: Some(diag),
        web_live: None,
        web_alive: None,
        video_frames: None,
        video_codec: None,
        hwdec: None,
        notes,
        error: render_err,
    }
}

fn verify_web(hgl: &HeadlessGl, wp: &Wallpaper, opts: &VerifyOptions) -> VerifyItemReport {
    let id = item_id(wp);
    let t_load = Instant::now();
    let mut renderer = match WebRenderer::new(
        &hgl.gl,
        &wp.root,
        &wp.entry,
        hgl.width,
        hgl.height,
        WebOptions {
            scale: 1.0,
            fps: 15,
        },
        wp.project.as_ref(),
    ) {
        Ok(r) => r,
        Err(err) => {
            let gl_errors = hgl.drain_gl_errors();
            trim_allocator();
            let (rss_mb, hwm_mb) = read_proc_rss_mb();
            return VerifyItemReport {
                id,
                title: wp.title.clone(),
                kind: wp.kind.as_str().to_string(),
                path: wp.entry.display().to_string(),
                status: VerifyStatus::Fail,
                load_ms: t_load.elapsed().as_millis() as u64,
                render_ms: 0,
                rss_mb,
                hwm_mb,
                gl_errors,
                frame: None,
                scene: None,
                web_live: None,
                web_alive: None,
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: Vec::new(),
                error: Some(format!("load failed: {err:#}")),
            };
        }
    };
    let load_ms = t_load.elapsed().as_millis() as u64;

    let t_render = Instant::now();
    let timeout = Duration::from_millis(opts.web_timeout_ms.max(500));
    let mut frame_idx = 0u64;
    let mut render_err: Option<String> = None;
    while t_render.elapsed() < timeout {
        let info = synthetic_frame(hgl.width, hgl.height, frame_idx, t_render.elapsed().as_secs_f64());
        frame_idx += 1;
        if let Err(err) = renderer.render(&hgl.gl, &info) {
            render_err = Some(format!("render failed: {err:#}"));
            break;
        }
        if renderer.has_live_frame() || !renderer.worker_alive() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if render_err.is_none() && renderer.has_live_frame() {
        for _ in 0..2 {
            std::thread::sleep(Duration::from_millis(50));
            let info =
                synthetic_frame(hgl.width, hgl.height, frame_idx, t_render.elapsed().as_secs_f64());
            frame_idx += 1;
            if let Err(err) = renderer.render(&hgl.gl, &info) {
                render_err = Some(format!("render failed: {err:#}"));
                break;
            }
        }
    }

    let web_live = renderer.has_live_frame();
    let web_alive = renderer.worker_alive();
    let gl_errors = hgl.drain_gl_errors();
    let rgba = hgl.read_rgba();
    let stats = compute_frame_stats(&rgba);
    let mut notes = Vec::new();
    save_dump_png(opts.dump_dir.as_deref(), &id, hgl.width, hgl.height, rgba, &mut notes);
    let render_ms = t_render.elapsed().as_millis() as u64;

    renderer.destroy(&hgl.gl);
    drop(renderer);
    unsafe { hgl.gl.finish() };
    trim_allocator();
    let (rss_mb, hwm_mb) = read_proc_rss_mb();

    let mut warn = check_rss_limit(opts.rss_limit_mb, rss_mb, &mut notes);
    if !web_live {
        notes.push("no live WebKit frame before timeout".to_string());
        warn = true;
    }
    if stats.non_black_ratio < 0.001 {
        notes.push(format!("near-black frame ({:.4})", stats.non_black_ratio));
        warn = true;
    }

    let status = if render_err.is_some() || !gl_errors.is_empty() || (!web_alive && !web_live) {
        if !web_alive && !web_live {
            notes.push("WebKit worker exited early".to_string());
        }
        VerifyStatus::Fail
    } else if warn {
        VerifyStatus::Warn
    } else {
        VerifyStatus::Ok
    };

    VerifyItemReport {
        id,
        title: wp.title.clone(),
        kind: wp.kind.as_str().to_string(),
        path: wp.entry.display().to_string(),
        status,
        load_ms,
        render_ms,
        rss_mb,
        hwm_mb,
        gl_errors,
        frame: Some(stats),
        scene: None,
        web_live: Some(web_live),
        web_alive: Some(web_alive),
        video_frames: None,
        video_codec: None,
        hwdec: None,
        notes,
        error: render_err,
    }
}

fn verify_video(hgl: &HeadlessGl, wp: &Wallpaper, opts: &VerifyOptions) -> VerifyItemReport {
    let id = item_id(wp);
    let t_load = Instant::now();
    let mut renderer = match VideoRenderer::new(&wp.entry, hgl.width, hgl.height, &opts.hwdec, 0.0, true)
    {
        Ok(r) => r,
        Err(err) => {
            let gl_errors = hgl.drain_gl_errors();
            trim_allocator();
            let (rss_mb, hwm_mb) = read_proc_rss_mb();
            return VerifyItemReport {
                id,
                title: wp.title.clone(),
                kind: wp.kind.as_str().to_string(),
                path: wp.entry.display().to_string(),
                status: VerifyStatus::Fail,
                load_ms: t_load.elapsed().as_millis() as u64,
                render_ms: 0,
                rss_mb,
                hwm_mb,
                gl_errors,
                frame: None,
                scene: None,
                web_live: None,
                web_alive: None,
                video_frames: Some(0),
                video_codec: None,
                hwdec: None,
                notes: Vec::new(),
                error: Some(format!("load failed: {err:#}")),
            };
        }
    };
    let load_ms = t_load.elapsed().as_millis() as u64;

    let t_render = Instant::now();
    let timeout = Duration::from_millis(opts.video_timeout_ms.max(500));
    let mut frame_idx = 0u64;
    let mut render_err: Option<String> = None;
    while t_render.elapsed() < timeout {
        let info = synthetic_frame(hgl.width, hgl.height, frame_idx, t_render.elapsed().as_secs_f64());
        frame_idx += 1;
        if let Err(err) = renderer.render(&hgl.gl, &info) {
            render_err = Some(format!("render failed: {err:#}"));
            break;
        }
        if renderer.frames_drawn() >= 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let video_frames = renderer.frames_drawn();
    let (video_codec, hwdec) = renderer.video_info();
    let gl_errors = hgl.drain_gl_errors();
    let rgba = hgl.read_rgba();
    let stats = compute_frame_stats(&rgba);
    let mut notes = Vec::new();
    save_dump_png(opts.dump_dir.as_deref(), &id, hgl.width, hgl.height, rgba, &mut notes);
    let render_ms = t_render.elapsed().as_millis() as u64;

    renderer.destroy(&hgl.gl);
    drop(renderer);
    unsafe { hgl.gl.finish() };
    trim_allocator();
    let (rss_mb, hwm_mb) = read_proc_rss_mb();

    let mut warn = check_rss_limit(opts.rss_limit_mb, rss_mb, &mut notes);
    if video_frames < 2 {
        notes.push(format!("only {video_frames} decoded video frame(s) drawn"));
    }
    if stats.non_black_ratio < 0.001 {
        notes.push(format!("near-black frame ({:.4})", stats.non_black_ratio));
        warn = true;
    }

    let status = if render_err.is_some() || !gl_errors.is_empty() || video_frames < 2 {
        VerifyStatus::Fail
    } else if warn {
        VerifyStatus::Warn
    } else {
        VerifyStatus::Ok
    };

    VerifyItemReport {
        id,
        title: wp.title.clone(),
        kind: wp.kind.as_str().to_string(),
        path: wp.entry.display().to_string(),
        status,
        load_ms,
        render_ms,
        rss_mb,
        hwm_mb,
        gl_errors,
        frame: Some(stats),
        scene: None,
        web_live: None,
        web_alive: None,
        video_frames: Some(video_frames),
        video_codec,
        hwdec,
        notes,
        error: render_err,
    }
}

fn verify_wallpaper(hgl: &HeadlessGl, wp: &Wallpaper, opts: &VerifyOptions) -> VerifyItemReport {
    hgl.reset_framebuffer();
    match wp.kind {
        WallpaperKind::Scene => verify_scene(hgl, wp, opts),
        WallpaperKind::Web => verify_web(hgl, wp, opts),
        WallpaperKind::Video => verify_video(hgl, wp, opts),
        WallpaperKind::Image => {
            let id = item_id(wp);
            let t_load = Instant::now();
            match ImageRenderer::new(&hgl.gl, &wp.entry, hgl.width, hgl.height) {
                Ok(r) => verify_static_renderer(
                    hgl,
                    Box::new(r),
                    opts,
                    &id,
                    t_load.elapsed().as_millis() as u64,
                    wp,
                ),
                Err(err) => {
                    let (rss_mb, hwm_mb) = read_proc_rss_mb();
                    VerifyItemReport {
                        id,
                        title: wp.title.clone(),
                        kind: wp.kind.as_str().to_string(),
                        path: wp.entry.display().to_string(),
                        status: VerifyStatus::Fail,
                        load_ms: t_load.elapsed().as_millis() as u64,
                        render_ms: 0,
                        rss_mb,
                        hwm_mb,
                        gl_errors: hgl.drain_gl_errors(),
                        frame: None,
                        scene: None,
                        web_live: None,
                        web_alive: None,
                        video_frames: None,
                        video_codec: None,
                        hwdec: None,
                        notes: Vec::new(),
                        error: Some(format!("load failed: {err:#}")),
                    }
                }
            }
        }
        WallpaperKind::Shader => {
            let id = item_id(wp);
            let t_load = Instant::now();
            match ShaderRenderer::new(&hgl.gl, &wp.entry, hgl.width, hgl.height) {
                Ok(r) => verify_static_renderer(
                    hgl,
                    Box::new(r),
                    opts,
                    &id,
                    t_load.elapsed().as_millis() as u64,
                    wp,
                ),
                Err(err) => {
                    let (rss_mb, hwm_mb) = read_proc_rss_mb();
                    VerifyItemReport {
                        id,
                        title: wp.title.clone(),
                        kind: wp.kind.as_str().to_string(),
                        path: wp.entry.display().to_string(),
                        status: VerifyStatus::Fail,
                        load_ms: t_load.elapsed().as_millis() as u64,
                        render_ms: 0,
                        rss_mb,
                        hwm_mb,
                        gl_errors: hgl.drain_gl_errors(),
                        frame: None,
                        scene: None,
                        web_live: None,
                        web_alive: None,
                        video_frames: None,
                        video_codec: None,
                        hwdec: None,
                        notes: Vec::new(),
                        error: Some(format!("load failed: {err:#}")),
                    }
                }
            }
        }
        WallpaperKind::Application => {
            let (rss_mb, hwm_mb) = read_proc_rss_mb();
            VerifyItemReport {
                id: item_id(wp),
                title: wp.title.clone(),
                kind: wp.kind.as_str().to_string(),
                path: wp.entry.display().to_string(),
                status: VerifyStatus::Skip,
                load_ms: 0,
                render_ms: 0,
                rss_mb,
                hwm_mb,
                gl_errors: Vec::new(),
                frame: None,
                scene: None,
                web_live: None,
                web_alive: None,
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: vec!["application wallpapers cannot be verified headless".to_string()],
                error: None,
            }
        }
    }
}

fn discover_wallpapers(opts: &VerifyOptions) -> Result<Vec<Wallpaper>> {
    let mut items = if let Some(dir) = &opts.dir {
        let is_single = dir.is_file()
            || dir.join("project.json").is_file()
            || dir.join("wallpaper.toml").is_file()
            || dir.join("scene.toml").is_file();
        if is_single {
            vec![Wallpaper::resolve(dir)?]
        } else {
            let scanned = wallpaper::scan(dir);
            let mut list = Vec::with_capacity(scanned.len());
            for (_, path) in scanned {
                if let Ok(wp) = Wallpaper::resolve(&path) {
                    list.push(wp);
                }
            }
            if list.is_empty() {
                if let Ok(wp) = Wallpaper::resolve(dir) {
                    list.push(wp);
                }
            }
            list
        }
    } else {
        wallpaper::scan_workshop()
    };

    if let Some(kind) = opts.kind {
        items.retain(|w| w.kind == kind);
    }
    if let Some(filter) = opts.filter.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let q = filter.to_ascii_lowercase();
        items.retain(|w| {
            w.workshop_id
                .as_deref()
                .is_some_and(|id| id.to_ascii_lowercase().contains(&q))
                || w.title.to_ascii_lowercase().contains(&q)
                || w.root.display().to_string().to_ascii_lowercase().contains(&q)
        });
    }
    items.sort_by(|a, b| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then_with(|| a.workshop_id.cmp(&b.workshop_id))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok(items)
}

fn print_item_progress(idx: usize, total: usize, item: &VerifyItemReport) {
    let mut detail = String::new();
    if let Some(s) = &item.scene {
        detail = format!(
            "obj={:<3} eff={:<3} shd_fail={} scr={}/{}",
            s.objects,
            s.effects_loaded,
            s.shader_failures.len(),
            s.script_failures,
            s.script_bindings
        );
    } else if let Some(live) = item.web_live {
        detail = format!("live={live} alive={}", item.web_alive.unwrap_or(false));
    } else if let Some(vf) = item.video_frames {
        detail = format!(
            "frames={vf} codec={} hwdec={}",
            item.video_codec.as_deref().unwrap_or("-"),
            item.hwdec.as_deref().unwrap_or("-")
        );
    }
    let cov = item
        .frame
        .as_ref()
        .map(|f| format!("cov={:.0}%", f.non_black_ratio * 100.0))
        .unwrap_or_else(|| "cov=-".to_string());
    let extra = item
        .error
        .as_deref()
        .or_else(|| item.notes.first().map(String::as_str))
        .unwrap_or("");
    let title: String = item.title.chars().take(28).collect();
    println!(
        "[{:>2}/{:<2}] [{}] {:<5} {:<11} {:<28} {:>5}+{:>5}ms {:>6.1}M {} {} {}",
        idx + 1,
        total,
        item.status.tag(),
        item.kind,
        item.id,
        title,
        item.load_ms,
        item.render_ms,
        item.rss_mb,
        cov,
        detail,
        extra
    );
    let _ = std::io::stdout().flush();
}

/// Run headless offscreen regression verification across discovered wallpapers.
pub fn run_verify(opts: &VerifyOptions) -> Result<VerifyReport> {
    let started = Instant::now();
    let hgl = HeadlessGl::new(opts.width, opts.height)?;
    let items_to_run = discover_wallpapers(opts)?;
    if !opts.json {
        println!(
            "anima verify: {} item(s) @ {}x{} ({} | {})",
            items_to_run.len(),
            hgl.width,
            hgl.height,
            hgl.renderer_name,
            hgl.gl_version
        );
    }
    let total = items_to_run.len();
    let mut results = Vec::with_capacity(total);
    for (idx, wp) in items_to_run.iter().enumerate() {
        let rep = verify_wallpaper(&hgl, wp, opts);
        if !opts.json {
            print_item_progress(idx, total, &rep);
        }
        results.push(rep);
    }
    Ok(VerifyReport::from_items(
        hgl.renderer_name.clone(),
        hgl.gl_version.clone(),
        started.elapsed().as_millis() as u64,
        results,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_stats_detects_black_and_textured_frames() {
        let black = vec![0u8, 0, 0, 255, 2, 1, 3, 255, 0, 0, 0, 0];
        let s_black = compute_frame_stats(&black);
        assert!(s_black.non_black_ratio < 0.01);
        assert!((s_black.transparent_ratio - 1.0 / 3.0).abs() < 1e-4);
        assert!(s_black.mean_luma < 0.02);

        let lit = vec![255u8, 255, 255, 255, 0, 0, 0, 255];
        let s_lit = compute_frame_stats(&lit);
        assert!((s_lit.non_black_ratio - 0.5).abs() < 1e-4);
        assert!((s_lit.mean_luma - 0.5).abs() < 1e-4);
        assert!((s_lit.max_luma - 1.0).abs() < 1e-4);
        assert!((s_lit.luma_std - 0.5).abs() < 1e-4);
        assert!(s_lit.transparent_ratio.abs() < 1e-6);
    }

    #[test]
    fn parse_proc_status_rss_extracts_rss_and_hwm() {
        let sample = "\
Name:\tanima
State:\tR (running)
VmPeak:\t 1234560 kB
VmHWM:\t  524288 kB
VmRSS:\t  262144 kB
Threads:\t4
";
        let (rss, hwm) = parse_proc_status_rss_mb(sample);
        assert!((rss - 256.0).abs() < 1e-6);
        assert!((hwm - 512.0).abs() < 1e-6);
        let (z_rss, z_hwm) = parse_proc_status_rss_mb("unrelated: 1 kB\n");
        assert_eq!((z_rss, z_hwm), (0.0, 0.0));
    }

    #[test]
    fn report_summary_aggregates_counts_and_failures() {
        let dummy_scene = SceneDiagnostics {
            shader_failures: vec!["bad.frag".into()],
            script_failures: 2,
            ..Default::default()
        };
        let items = vec![
            VerifyItemReport {
                id: "1".into(),
                title: "ok".into(),
                kind: "scene".into(),
                path: "/1".into(),
                status: VerifyStatus::Ok,
                load_ms: 10,
                render_ms: 20,
                rss_mb: 120.0,
                hwm_mb: 140.0,
                gl_errors: vec![],
                frame: None,
                scene: Some(SceneDiagnostics::default()),
                web_live: None,
                web_alive: None,
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: vec![],
                error: None,
            },
            VerifyItemReport {
                id: "2".into(),
                title: "warn".into(),
                kind: "web".into(),
                path: "/2".into(),
                status: VerifyStatus::Warn,
                load_ms: 5,
                render_ms: 100,
                rss_mb: 220.0,
                hwm_mb: 250.0,
                gl_errors: vec![],
                frame: None,
                scene: None,
                web_live: Some(false),
                web_alive: Some(true),
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: vec!["timeout".into()],
                error: None,
            },
            VerifyItemReport {
                id: "3".into(),
                title: "fail".into(),
                kind: "scene".into(),
                path: "/3".into(),
                status: VerifyStatus::Fail,
                load_ms: 15,
                render_ms: 30,
                rss_mb: 180.0,
                hwm_mb: 310.0,
                gl_errors: vec![0x0502],
                frame: None,
                scene: Some(dummy_scene),
                web_live: None,
                web_alive: None,
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: vec![],
                error: Some("shader failed".into()),
            },
            VerifyItemReport {
                id: "4".into(),
                title: "skip".into(),
                kind: "application".into(),
                path: "/4".into(),
                status: VerifyStatus::Skip,
                load_ms: 0,
                render_ms: 0,
                rss_mb: 50.0,
                hwm_mb: 50.0,
                gl_errors: vec![],
                frame: None,
                scene: None,
                web_live: None,
                web_alive: None,
                video_frames: None,
                video_codec: None,
                hwdec: None,
                notes: vec![],
                error: None,
            },
        ];
        let rep = VerifyReport::from_items("Mesa".into(), "ES 3.2".into(), 999, items);
        assert_eq!(rep.total, 4);
        assert_eq!(rep.passed, 1);
        assert_eq!(rep.warned, 1);
        assert_eq!(rep.failed, 1);
        assert_eq!(rep.skipped, 1);
        assert!((rep.peak_rss_mb - 220.0).abs() < 1e-6);
        assert!((rep.peak_hwm_mb - 310.0).abs() < 1e-6);
        assert_eq!(rep.total_shader_failures, 1);
        assert_eq!(rep.total_script_failures, 2);
        assert_eq!(rep.total_gl_errors, 1);
    }

    #[test]
    #[ignore = "requires GPU and local Steam Workshop library"]
    fn workshop_full_regression() {
        let rep = run_verify(&VerifyOptions::default()).expect("run_verify succeeds");
        assert!(rep.total > 0, "no Workshop wallpapers found");
        assert_eq!(
            rep.failed, 0,
            "{} wallpaper(s) failed regression verification",
            rep.failed
        );
    }
}
