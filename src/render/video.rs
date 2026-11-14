//! Video wallpapers rendered with libmpv's OpenGL render API.
//!
//! Using mpv (instead of a hand rolled ffmpeg pipeline) gives us hardware
//! decoding (VAAPI/NVDEC), zero-copy GL interop, correct colour handling and
//! robust container support with very little code.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use anyhow::{anyhow, bail, Result};
use glow::HasContext;

use super::{FrameInfo, Renderer};

#[allow(non_camel_case_types)]
type mpv_handle = c_void;
#[allow(non_camel_case_types)]
type mpv_render_context = c_void;

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvRenderParam {
    type_: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvOpenglInitParams {
    get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    get_proc_address_ctx: *mut c_void,
}

#[repr(C)]
struct MpvOpenglFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[link(name = "mpv")]
unsafe extern "C" {
    fn mpv_create() -> *mut mpv_handle;
    fn mpv_initialize(handle: *mut mpv_handle) -> c_int;
    fn mpv_terminate_destroy(handle: *mut mpv_handle);
    fn mpv_set_option_string(handle: *mut mpv_handle, name: *const c_char, value: *const c_char) -> c_int;
    fn mpv_set_property_string(handle: *mut mpv_handle, name: *const c_char, value: *const c_char) -> c_int;
    fn mpv_command(handle: *mut mpv_handle, args: *const *const c_char) -> c_int;
    fn mpv_wait_event(handle: *mut mpv_handle, timeout: f64) -> *mut MpvEvent;
    fn mpv_error_string(error: c_int) -> *const c_char;
    fn mpv_get_property_string(handle: *mut mpv_handle, name: *const c_char) -> *mut c_char;
    fn mpv_free(data: *mut c_void);

    fn mpv_render_context_create(
        ctx: *mut *mut mpv_render_context,
        handle: *mut mpv_handle,
        params: *mut MpvRenderParam,
    ) -> c_int;
    fn mpv_render_context_render(
        ctx: *mut mpv_render_context,
        params: *mut MpvRenderParam,
    ) -> c_int;
    fn mpv_render_context_update(ctx: *mut mpv_render_context) -> u64;
    fn mpv_render_context_report_swap(ctx: *mut mpv_render_context);
    fn mpv_render_context_set_update_callback(
        ctx: *mut mpv_render_context,
        callback: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
    );
    fn mpv_render_context_free(ctx: *mut mpv_render_context);
}

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetProcAddress(name: *const c_char) -> *mut c_void;
}

unsafe extern "C" fn get_proc_address(_ctx: *mut c_void, name: *const c_char) -> *mut c_void {
    unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, name);
        if !symbol.is_null() {
            symbol
        } else {
            eglGetProcAddress(name)
        }
    }
}

unsafe extern "C" fn update_callback(_data: *mut c_void) {
    // Advanced control mode requires a callback, but our compositor driven
    // frame loop polls `mpv_render_context_update` every frame instead of
    // waking a thread. Doing nothing here avoids cross-thread wakeups.
}

const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
const MPV_RENDER_PARAM_WL_DISPLAY: c_int = 9;
const MPV_RENDER_PARAM_ADVANCED_CONTROL: c_int = 10;
const MPV_RENDER_UPDATE_FRAME: u64 = 1;

pub struct VideoRenderer {
    handle: *mut mpv_handle,
    render_ctx: *mut mpv_render_context,
    path: PathBuf,
    width: i32,
    height: i32,
    first_frame: bool,
    frames_drawn: u32,
    decoder_logged: bool,
    paused: bool,
    volume: f32,
    muted: bool,
}

impl VideoRenderer {
    pub fn new(
        path: &Path,
        width: i32,
        height: i32,
        hwdec: &str,
        _wl_display: *mut c_void,
        volume: f32,
        muted: bool,
    ) -> Result<Self> {
        unsafe {
            let handle = mpv_create();
            if handle.is_null() {
                bail!("mpv_create() failed");
            }

            let opts: &[(&str, &str)] = &[
                ("vo", "libmpv"),
                ("hwdec", hwdec),
                ("loop-file", "inf"),
                ("keep-open", "yes"),
                ("idle", "yes"),
                ("panscan", "1.0"),
                ("keepaspect", "yes"),
                ("osc", "no"),
                ("osd-level", "0"),
                ("input-default-bindings", "no"),
                ("input-vo-keyboard", "no"),
                ("terminal", "no"),
                ("msg-level", "all=warn"),
                ("ytdl", "no"),
                ("audio-display", "no"),
                // Keep memory bounded for long 4K videos.
                ("demuxer-max-bytes", "256MiB"),
                ("demuxer-max-back-bytes", "64MiB"),
            ];

            for (name, value) in opts {
                let cname = CString::new(*name)?;
                let cvalue = CString::new(*value)?;
                let rc = mpv_set_option_string(handle, cname.as_ptr(), cvalue.as_ptr());
                if rc < 0 {
                    log::warn!(
                        "mpv option {name}={value} rejected: {}",
                        cstr(mpv_error_string(rc))
                    );
                }
            }

            let rc = mpv_initialize(handle);
            if rc < 0 {
                let msg = cstr(mpv_error_string(rc));
                mpv_terminate_destroy(handle);
                bail!("mpv_initialize failed: {msg}");
            }

            let mut this = Self {
                handle,
                render_ctx: ptr::null_mut(),
                path: path.to_path_buf(),
                width,
                height,
                first_frame: true,
                frames_drawn: 0,
                decoder_logged: false,
                paused: false,
                volume,
                muted,
            };

            this.apply_audio();

            // Build the render context.
            let api = c"opengl";
            let mut init = MpvOpenglInitParams {
                get_proc_address: Some(get_proc_address),
                get_proc_address_ctx: ptr::null_mut(),
            };
            let mut advanced: c_int = 1;
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_API_TYPE,
                    data: api.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                    data: &mut init as *mut _ as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_ADVANCED_CONTROL,
                    data: &mut advanced as *mut _ as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_WL_DISPLAY,
                    // Unlike the int parameters, mpv stores this one by value:
                    // `data` itself is the `struct wl_display*`.
                    //
                    // Disabled for now: libva's vaGetDisplayWl() drives the
                    // compositor connection from mpv's own threads, which
                    // corrupts the connection our event loop owns (Hyprland
                    // then rejects wl_surface.attach). Hardware decoding is
                    // still available through EGL dmabuf import.
                    data: ptr::null_mut(),
                },
                MpvRenderParam {
                    type_: 0,
                    data: ptr::null_mut(),
                },
            ];

            let mut ctx: *mut mpv_render_context = ptr::null_mut();
            let rc = mpv_render_context_create(&mut ctx, handle, params.as_mut_ptr());
            if rc < 0 {
                let msg = cstr(mpv_error_string(rc));
                mpv_terminate_destroy(handle);
                bail!("mpv_render_context_create failed: {msg}");
            }
            this.render_ctx = ctx;
            mpv_render_context_set_update_callback(ctx, Some(update_callback), ptr::null_mut());

            this.load()?;
            Ok(this)
        }
    }

    unsafe fn load(&mut self) -> Result<()> {
        let cmd = CString::new("loadfile")?;
        let path = CString::new(self.path.to_string_lossy().as_bytes())?;
        let args = [cmd.as_ptr(), path.as_ptr(), ptr::null()];
        let rc = unsafe { mpv_command(self.handle, args.as_ptr()) };
        if rc < 0 {
            bail!(
                "mpv loadfile {} failed: {}",
                self.path.display(),
                unsafe { cstr(mpv_error_string(rc)) }
            );
        }
        self.first_frame = true;
        Ok(())
    }

    fn apply_audio(&mut self) {
        let mute = if self.muted { "yes" } else { "no" };
        let volume = (self.volume.clamp(0.0, 1.0) * 100.0).round();
        unsafe {
            let _ = set_prop(self.handle, "mute", mute);
            let _ = set_prop(self.handle, "volume", &volume.to_string());
        }
    }

    /// Report the decoder that mpv actually picked (useful when hwdec
    /// silently falls back to software).
    fn log_decoder(&self) {
        unsafe {
            let mut parts = Vec::new();
            for prop in ["hwdec-current", "video-codec"] {
                let Ok(name) = CString::new(prop) else { continue };
                let value = mpv_get_property_string(self.handle, name.as_ptr());
                if value.is_null() {
                    parts.push(format!("{prop}=<unavailable>"));
                    continue;
                }
                parts.push(format!("{prop}={}", cstr(value)));
                mpv_free(value as *mut c_void);
            }
            if !parts.is_empty() {
                log::info!("video {}: {}", self.path.display(), parts.join(" "));
            }
        }
    }

    fn drain_events(&mut self) {
        unsafe {
            for _ in 0..64 {
                let event = mpv_wait_event(self.handle, 0.0);
                if event.is_null() {
                    break;
                }
                if (*event).event_id == 0 {
                    // MPV_EVENT_NONE: queue is drained.
                    break;
                }
            }
        }
    }
}

impl Renderer for VideoRenderer {
    fn render(&mut self, gl: &glow::Context, _info: &FrameInfo) -> Result<bool> {
        self.drain_events();
        unsafe {
            let flags = if self.first_frame {
                MPV_RENDER_UPDATE_FRAME
            } else {
                mpv_render_context_update(self.render_ctx)
            };
            if flags & MPV_RENDER_UPDATE_FRAME == 0 {
                return Ok(true);
            }

            gl.viewport(0, 0, self.width, self.height);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            let mut fbo = MpvOpenglFbo {
                fbo: 0,
                w: self.width,
                h: self.height,
                internal_format: glow::RGBA8 as c_int,
            };
            let mut flip: c_int = 1;
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_OPENGL_FBO,
                    data: &mut fbo as *mut _ as *mut c_void,
                },
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_FLIP_Y,
                    data: &mut flip as *mut _ as *mut c_void,
                },
                MpvRenderParam {
                    type_: 0,
                    data: ptr::null_mut(),
                },
            ];
            let rc = mpv_render_context_render(self.render_ctx, params.as_mut_ptr());
            if rc < 0 {
                log::warn!("mpv render failed: {}", cstr(mpv_error_string(rc)));
            }
            mpv_render_context_report_swap(self.render_ctx);
            if self.first_frame {
                self.first_frame = false;
            }
            self.frames_drawn += 1;
            // The decoder properties are only populated once playback has
            // actually started.
            if !self.decoder_logged && self.frames_drawn >= 15 {
                self.log_decoder();
                self.decoder_logged = true;
            }
        }
        Ok(true)
    }

    fn resize(&mut self, _gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        unsafe {
            let _ = set_prop(self.handle, "pause", if paused { "yes" } else { "no" });
        }
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        let volume = (self.volume * 100.0).round();
        unsafe {
            let _ = set_prop(self.handle, "volume", &volume.to_string());
        }
    }

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        unsafe {
            match key.to_ascii_lowercase().as_str() {
                "speed" => {
                    if let Ok(v) = value.parse::<f32>() {
                        set_prop(self.handle, "speed", &v.clamp(0.1, 4.0).to_string())?;
                    }
                }
                "volume" => {
                    if let Ok(v) = value.parse::<f32>() {
                        let norm = if v > 1.0 { v / 100.0 } else { v };
                        self.set_volume(norm);
                    }
                }
                "mute" | "muted" => {
                    self.muted =
                        value.eq_ignore_ascii_case("true") || value == "1" || value.eq_ignore_ascii_case("yes");
                    self.apply_audio();
                }
                "ui_brightness" => {
                    if let Ok(v) = value.parse::<f32>() {
                        let mpv = (v.clamp(-1.0, 1.0) * 100.0).round();
                        set_prop(self.handle, "brightness", &mpv.to_string())?;
                    }
                }
                "ui_contrast" => {
                    if let Ok(v) = value.parse::<f32>() {
                        let c = v.clamp(0.0, 3.0);
                        let mpv = if c >= 1.0 { (c - 1.0) * 50.0 } else { (c - 1.0) * 100.0 };
                        set_prop(self.handle, "contrast", &mpv.clamp(-100.0, 100.0).round().to_string())?;
                    }
                }
                "ui_saturation" => {
                    if let Ok(v) = value.parse::<f32>() {
                        let s = v.clamp(0.0, 3.0);
                        let mpv = if s >= 1.0 { (s - 1.0) * 50.0 } else { (s - 1.0) * 100.0 };
                        set_prop(self.handle, "saturation", &mpv.clamp(-100.0, 100.0).round().to_string())?;
                    }
                }
                "ui_hue" | "hue" | "hueshift" => {
                    if let Ok(v) = value.parse::<f32>() {
                        let mpv = (v.clamp(-180.0, 180.0) / 180.0 * 100.0).round();
                        set_prop(self.handle, "hue", &mpv.to_string())?;
                    }
                }
                "brightness" | "contrast" | "saturation" | "gamma" | "panscan" => {
                    set_prop(self.handle, &key.to_ascii_lowercase(), value)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("video {}", self.path.display())
    }
}

impl Drop for VideoRenderer {
    fn drop(&mut self) {
        unsafe {
            if !self.render_ctx.is_null() {
                mpv_render_context_free(self.render_ctx);
                self.render_ctx = ptr::null_mut();
            }
            if !self.handle.is_null() {
                mpv_terminate_destroy(self.handle);
                self.handle = ptr::null_mut();
            }
        }
    }
}

unsafe fn set_prop(handle: *mut mpv_handle, name: &str, value: &str) -> Result<()> {
    let name = CString::new(name)?;
    let value = CString::new(value)?;
    let rc = unsafe { mpv_set_property_string(handle, name.as_ptr(), value.as_ptr()) };
    if rc < 0 {
        return Err(anyhow!(
            "mpv property {} = {} failed: {}",
            name.to_string_lossy(),
            value.to_string_lossy(),
            unsafe { cstr(mpv_error_string(rc)) }
        ));
    }
    Ok(())
}

unsafe fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return "<unknown>".into();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

/// Headless libmpv audio player used for Wallpaper Engine scene `sound`
/// layers (background music bundled with a scene).  Wallpaper Engine plays
/// these and its own audio visualiser reacts to them, so audio-reactive
/// effects (pulses, glitch transitions) depend on the music actually playing.
pub struct AudioPlayer {
    handle: *mut mpv_handle,
    volume: f32,
    muted: bool,
}

impl AudioPlayer {
    pub fn new(files: &[PathBuf], shuffle: bool, volume: f32, muted: bool) -> Result<Self> {
        if files.is_empty() {
            bail!("no audio files");
        }
        unsafe {
            let handle = mpv_create();
            if handle.is_null() {
                bail!("mpv_create() failed");
            }
            let opts: &[(&str, &str)] = &[
                ("vid", "no"),
                ("video", "no"),
                ("vo", "null"),
                ("loop-playlist", "inf"),
                ("idle", "yes"),
                ("terminal", "no"),
                ("msg-level", "all=warn"),
                ("ytdl", "no"),
                ("audio-display", "no"),
                ("input-default-bindings", "no"),
                ("shuffle", if shuffle { "yes" } else { "no" }),
                ("audio-client-name", "anima"),
            ];
            for (name, value) in opts {
                let cname = CString::new(*name)?;
                let cvalue = CString::new(*value)?;
                mpv_set_option_string(handle, cname.as_ptr(), cvalue.as_ptr());
            }
            let rc = mpv_initialize(handle);
            if rc < 0 {
                let msg = cstr(mpv_error_string(rc));
                mpv_terminate_destroy(handle);
                bail!("mpv_initialize failed: {msg}");
            }
            let this = Self { handle, volume, muted };
            this.apply();
            for (i, f) in files.iter().enumerate() {
                let path = CString::new(f.to_string_lossy().as_bytes())?;
                let cmd = c"loadfile";
                let mode = if i == 0 { c"replace" } else { c"append" };
                let args = [cmd.as_ptr(), path.as_ptr(), mode.as_ptr(), ptr::null()];
                mpv_command(handle, args.as_ptr());
            }
            Ok(this)
        }
    }

    fn apply(&self) {
        let volume = (self.volume.clamp(0.0, 1.0) * 100.0).round();
        unsafe {
            let _ = set_prop(self.handle, "mute", if self.muted { "yes" } else { "no" });
            let _ = set_prop(self.handle, "volume", &volume.to_string());
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        unsafe {
            let _ = set_prop(self.handle, "pause", if paused { "yes" } else { "no" });
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.apply();
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
        self.apply();
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_null() {
                mpv_terminate_destroy(self.handle);
                self.handle = ptr::null_mut();
            }
        }
    }
}
