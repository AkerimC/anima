//! Web wallpaper renderer backed by an offscreen WebKit2GTK process.
//!
//! Renders real Wallpaper Engine HTML/CSS/JS/WebGL wallpapers (such as `index.html`)
//! in a headless `Gtk.OffscreenWindow` with `WebKit2.WebView` and streams live frames
//! into an OpenGL texture while forwarding `wallpaperPropertyListener` properties and
//! `wallpaperRegisterAudioListener` audio spectrum updates.
//!
//! If WebKit2GTK is unavailable at runtime, falls back cleanly to a lightweight
//! procedural shader backdrop so startup never fails.

use std::collections::HashMap;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
uniform int u_web_live;
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
uniform vec4 u_bcs_hue; // (brightness, contrast, saturation, hue_rad)

out vec4 fragColor;

float seg(vec2 p, vec2 a, vec2 b) {
    vec2 pa = p - a, ba = b - a;
    float h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

float digit_sdf(vec2 p, int d) {
    int masks[10] = int[10](
        0x77, 0x24, 0x5D, 0x6D, 0x2E, 0x6B, 0x7B, 0x25, 0x7F, 0x6F
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

vec3 apply_bcs_hue(vec3 col) {
    vec3 rgb = (col - 0.5) * u_bcs_hue.y + 0.5 + u_bcs_hue.x;
    float luma = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3(luma), rgb, u_bcs_hue.z);
    if (abs(u_bcs_hue.w) > 1e-4) {
        const vec3 k = vec3(0.577350269);
        float cosA = cos(u_bcs_hue.w);
        float sinA = sin(u_bcs_hue.w);
        rgb = rgb * cosA + cross(k, rgb) * sinA + k * dot(k, rgb) * (1.0 - cosA);
    }
    return clamp(rgb, 0.0, 1.0);
}

void main() {
    vec2 aspect = vec2(u_resolution.x / max(u_resolution.y, 1.0), 1.0);
    vec3 col;

    if (u_web_live == 1) {
        // Cairo ARGB32 on little-endian stores BGRA bytes in memory; swizzle BGR -> RGB.
        vec2 uv = vec2(v_uv.x, 1.0 - v_uv.y);
        vec3 bgr = texture(u_bg, uv).rgb;
        col = vec3(bgr.b, bgr.g, bgr.r);
    } else {
        vec2 mouse_uv = u_mouse / max(u_resolution, vec2(1.0));
        mouse_uv.y = 1.0 - mouse_uv.y;

        vec2 parallax = (mouse_uv - 0.5) * 0.025;
        vec2 bg_uv = vec2(v_uv.x, 1.0 - v_uv.y);
        bg_uv = (bg_uv - 0.5 + parallax) * u_uv_scale + 0.5;

        if (u_has_bg == 1) {
            col = texture(u_bg, clamp(bg_uv, 0.0, 1.0)).rgb;
            col *= 1.0 + u_audio.x * 0.12;
        } else {
            vec2 p = (v_uv - 0.5) * aspect;
            float w1 = sin(p.x * 3.0 + u_time * 0.7) * cos(p.y * 2.5 - u_time * 0.5);
            col = mix(vec3(0.03, 0.04, 0.08), u_scheme_color * 0.55, 0.5 + 0.5 * w1);
        }

        float m_dist = length((v_uv - mouse_uv) * aspect);
        col += u_scheme_color * (0.08 + u_audio.w * 0.15) * exp(-m_dist * 6.5);
    }

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
        float colon = min(length(cp - vec2(0.0, 0.30)), length(cp - vec2(0.0, -0.30)));
        d = min(d, colon);
        d = min(d, digit_sdf(cp - vec2(0.75, 0.0), d2));
        d = min(d, digit_sdf(cp - vec2(1.85, 0.0), d3));

        float stroke = smoothstep(0.095, 0.055, d);
        float glow = exp(-d * 5.0) * (0.35 + u_audio.x * 0.4);
        col = mix(col, vec3(0.96, 0.98, 1.0), stroke * 0.92);
        col += u_scheme_color * glow * 0.45;
    }

    fragColor = vec4(apply_bcs_hue(col), 1.0);
}
"#;

const WEBKIT_WORKER_SCRIPT: &str = r###"
import http.server
import json
import mimetypes
import os
import posixpath
import struct
import sys
import threading
import urllib.parse

# Preserve original stdout fd for binary frames; redirect fd 1 to stderr so no
# GTK/WebKit/Mesa C-level log line can corrupt the binary frame stream.
_frame_fd = os.dup(1)
os.dup2(2, 1)
_frame_out = os.fdopen(_frame_fd, "wb", buffering=0)

import gi
gi.require_version("Gdk", "3.0")
gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import GLib, Gtk, WebKit2

entry_abs = os.path.abspath(sys.argv[1])
root_dir = os.path.dirname(entry_abs)
entry_name = os.path.basename(entry_abs)

def _clamp_dims(w, h):
    w = max(64, int(w))
    h = max(64, int(h))
    scale = min(1920.0 / w, 1080.0 / h, 1.0)
    return max(64, int(round(w * scale))), max(64, int(round(h * scale)))

width, height = _clamp_dims(sys.argv[2], sys.argv[3])
init_props = json.loads(sys.argv[4]) if len(sys.argv) > 4 else {}

_EXTRA_MIME = {
    ".wasm": "application/wasm",
    ".data": "application/octet-stream",
    ".unityweb": "application/octet-stream",
    ".js": "application/javascript",
    ".mjs": "application/javascript",
    ".json": "application/json",
    ".mp3": "audio/mpeg",
    ".ogg": "audio/ogg",
    ".wav": "audio/wav",
    ".webm": "video/webm",
    ".mp4": "video/mp4",
    ".webp": "image/webp",
    ".woff2": "font/woff2",
    ".svg": "image/svg+xml",
}
for _ext, _mime in _EXTRA_MIME.items():
    mimetypes.add_type(_mime, _ext)

class _QuietHttpServer(http.server.ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True
    def handle_error(self, request, client_address):
        pass

class _AnimaWebHandler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        self._content_encoding = None
        super().__init__(*args, directory=root_dir, **kwargs)

    def guess_type(self, path):
        clean = path.split("?", 1)[0].split("#", 1)[0]
        low = clean.lower()
        self._content_encoding = None
        if low.endswith(".gz"):
            self._content_encoding = "gzip"
            inner = clean[:-3]
        elif low.endswith(".br"):
            self._content_encoding = "br"
            inner = clean[:-3]
        else:
            inner = clean
        _, ext = posixpath.splitext(inner.lower())
        if ext in _EXTRA_MIME:
            return _EXTRA_MIME[ext]
        return super().guess_type(inner)

    def end_headers(self):
        if getattr(self, "_content_encoding", None):
            self.send_header("Content-Encoding", self._content_encoding)
            self._content_encoding = None
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def log_message(self, format, *args):
        pass

_httpd = _QuietHttpServer(("127.0.0.1", 0), _AnimaWebHandler)
_port = _httpd.server_address[1]
threading.Thread(target=_httpd.serve_forever, daemon=True).start()
url = f"http://127.0.0.1:{_port}/{urllib.parse.quote(entry_name)}"

# OffscreenWindow without an external GL compositor needs software compositing
# in WebKit2GTK; WebGL remains enabled via ANGLE/llvmpipe.
os.environ.setdefault("WEBKIT_DISABLE_COMPOSITING_MODE", "0")

ucm = WebKit2.UserContentManager()
shim_js = r"""
(() => {
  if (window.__animaWeInstalled) return;
  window.__animaWeInstalled = true;
  window.__animaProps = __ANIMA_INIT_PROPS__;
  window.__animaAudioCbs = [];
  window.wallpaperRegisterAudioListener = function(cb) {
    if (typeof cb === 'function') window.__animaAudioCbs.push(cb);
  };
  window.wallpaperRegisterMediaStatusListener = function() {};
  window.wallpaperRegisterMediaPropertiesListener = function() {};
  window.wallpaperRegisterMediaPlaybackListener = function() {};
  window.wallpaperRegisterMediaThumbnailListener = function() {};
  window.wallpaperRegisterMediaTimelineListener = function() {};
  window.__animaApplyProps = function(props) {
    if (!props || typeof props !== 'object') return;
    Object.assign(window.__animaProps, props);
    const l = window.wallpaperPropertyListener;
    if (l && typeof l.applyUserProperties === 'function') {
      try { l.applyUserProperties(props); } catch (_e) {}
    }
  };
  window.__animaApplyGeneral = function() {
    const l = window.wallpaperPropertyListener;
    if (l && typeof l.applyGeneralProperties === 'function') {
      try { l.applyGeneralProperties({ fps: 30 }); } catch (_e) {}
    }
  };
  window.__animaPushAudio = function(arr) {
    for (let i = 0; i < window.__animaAudioCbs.length; i++) {
      try { window.__animaAudioCbs[i](arr); } catch (_e) {}
    }
  };
  function _makeMouseInit(x, y, dx, dy, down, isPointer) {
    const init = {
      clientX: x,
      clientY: y,
      screenX: x,
      screenY: y,
      movementX: dx,
      movementY: dy,
      button: 0,
      buttons: down ? 1 : 0,
      bubbles: true,
      cancelable: true,
      view: window,
    };
    if (isPointer) {
      init.pointerId = 1;
      init.pointerType = 'mouse';
      init.isPrimary = true;
    }
    return init;
  }
  function _emitPointerAndMouse(target, ptrType, mouseType, x, y, dx, dy, down) {
    if (typeof PointerEvent === 'function' && ptrType) {
      const pe = new PointerEvent(ptrType, _makeMouseInit(x, y, dx, dy, down, true));
      if (dx !== 0 && pe.movementX !== dx) {
        try {
          Object.defineProperty(pe, 'movementX', { value: dx });
          Object.defineProperty(pe, 'movementY', { value: dy });
        } catch (_e) {}
      }
      target.dispatchEvent(pe);
    }
    if (mouseType) {
      const me = new MouseEvent(mouseType, _makeMouseInit(x, y, dx, dy, down, false));
      if (dx !== 0 && me.movementX !== dx) {
        try {
          Object.defineProperty(me, 'movementX', { value: dx });
          Object.defineProperty(me, 'movementY', { value: dy });
        } catch (_e) {}
      }
      target.dispatchEvent(me);
    }
  }
  window.__animaMouseInput = function(nx, ny, down, downChanged) {
    try {
      const x = Math.round(nx * (window.innerWidth || 1));
      const y = Math.round(ny * (window.innerHeight || 1));
      const prevDown = !!window.__animaMouseWasDown;
      const dx = (typeof window.__animaLastMouseX === 'number') ? (x - window.__animaLastMouseX) : 0;
      const dy = (typeof window.__animaLastMouseY === 'number') ? (y - window.__animaLastMouseY) : 0;
      const moved = (dx !== 0 || dy !== 0);
      window.__animaLastMouseX = x;
      window.__animaLastMouseY = y;
      window.__animaMouseWasDown = !!down;
      const target = document.elementFromPoint(x, y)
        || document.querySelector('#unity-canvas')
        || document.querySelector('canvas')
        || document.body
        || document;
      if (downChanged) {
        if (moved) {
          _emitPointerAndMouse(target, 'pointermove', 'mousemove', x, y, dx, dy, prevDown);
        }
        if (down) {
          _emitPointerAndMouse(target, 'pointerdown', 'mousedown', x, y, 0, 0, true);
        } else {
          _emitPointerAndMouse(target, 'pointerup', 'mouseup', x, y, 0, 0, false);
          _emitPointerAndMouse(target, null, 'click', x, y, 0, 0, false);
        }
      } else {
        _emitPointerAndMouse(target, 'pointermove', 'mousemove', x, y, dx, dy, !!down);
      }
    } catch (_e) {}
  };
  window.__animaMouseMove = function(nx, ny) {
    window.__animaMouseInput(nx, ny, false, false);
  };
  function _patchAudioDecode(Ctor) {
    if (!Ctor || !Ctor.prototype || !Ctor.prototype.decodeAudioData || Ctor.prototype.__animaDecodePatched) return;
    const orig = Ctor.prototype.decodeAudioData;
    Ctor.prototype.__animaDecodePatched = true;
    Ctor.prototype.decodeAudioData = function() {
      try {
        const p = orig.apply(this, arguments);
        if (p && typeof p.catch === 'function') p.catch(function() {});
        return p;
      } catch (_e) {
        return Promise.resolve();
      }
    };
  }
  _patchAudioDecode(window.AudioContext);
  _patchAudioDecode(window.webkitAudioContext);
})();
""".replace("__ANIMA_INIT_PROPS__", json.dumps(init_props))
ucm.add_script(WebKit2.UserScript.new(
    shim_js,
    WebKit2.UserContentInjectedFrames.TOP_FRAME,
    WebKit2.UserScriptInjectionTime.START,
    None,
    None,
))

settings = WebKit2.Settings()
try:
    settings.set_hardware_acceleration_policy(WebKit2.HardwareAccelerationPolicy.NEVER)
except Exception:
    pass
for setter, val in (
    ("set_enable_webgl", True),
    ("set_enable_media_stream", False),
    ("set_allow_file_access_from_file_urls", True),
    ("set_allow_universal_access_from_file_urls", True),
    ("set_enable_javascript", True),
    ("set_enable_page_cache", False),
    ("set_enable_developer_extras", False),
):
    fn = getattr(settings, setter, None)
    if fn is not None:
        try:
            fn(val)
        except Exception:
            pass

win = Gtk.OffscreenWindow()
win.set_default_size(width, height)
win.set_size_request(width, height)

webview = WebKit2.WebView.new_with_user_content_manager(ucm)
webview.set_settings(settings)
webview.set_size_request(width, height)
win.add(webview)
win.show_all()

state = {
    "loaded": False,
    "paused": False,
    "in_flight": False,
    "needs_snap": False,
    "closed": False,
}

def on_snap(view, res, _data=None):
    state["in_flight"] = False
    if state["closed"]:
        return
    try:
        surf = view.get_snapshot_finish(res)
        if surf is None:
            return
        surf.flush()
        w = surf.get_width()
        h = surf.get_height()
        if w <= 0 or h <= 0:
            return
        stride = surf.get_stride()
        data = surf.get_data()
        header = struct.pack("<II", w, h)
        row_bytes = w * 4
        if stride == row_bytes:
            payload = bytes(memoryview(data)[:h * row_bytes])
        else:
            mv = memoryview(data)
            buf = bytearray(h * row_bytes)
            for y in range(h):
                s = y * stride
                d = y * row_bytes
                buf[d:d + row_bytes] = mv[s:s + row_bytes]
            payload = bytes(buf)
        _frame_out.write(header)
        _frame_out.write(payload)
    except BrokenPipeError:
        state["closed"] = True
        Gtk.main_quit()
    except Exception:
        pass

def trigger_snap():
    if state["closed"] or not state["loaded"] or state["in_flight"]:
        return False
    state["in_flight"] = True
    state["needs_snap"] = False
    webview.get_snapshot(
        WebKit2.SnapshotRegion.VISIBLE,
        WebKit2.SnapshotOptions.NONE,
        None,
        on_snap,
        None,
    )
    return True

def periodic_tick():
    if state["closed"]:
        return False
    if state["loaded"] and (not state["paused"] or state["needs_snap"]):
        trigger_snap()
    return True

def on_load_changed(_view, event):
    if event == WebKit2.LoadEvent.FINISHED:
        state["loaded"] = True
        props_js = (
            f"window.__animaApplyGeneral && window.__animaApplyGeneral();"
            f"window.__animaApplyProps && window.__animaApplyProps({json.dumps(init_props)});"
            f"requestAnimationFrame(() => requestAnimationFrame(() => {{}}));"
        )
        webview.run_javascript(props_js, None, None, None)
        GLib.timeout_add(120, lambda: (trigger_snap(), False)[1])

webview.connect("load-changed", on_load_changed)
webview.connect("permission-request", lambda _v, req: (req.deny(), True)[1])
webview.load_uri(url)
GLib.timeout_add(33, periodic_tick)

def handle_cmd(msg):
    if state["closed"]:
        return False
    cmd = msg.get("cmd")
    if cmd == "resize":
        w, h = _clamp_dims(msg.get("w", 1280), msg.get("h", 720))
        win.set_default_size(w, h)
        win.set_size_request(w, h)
        webview.set_size_request(w, h)
        win.queue_resize()
        state["needs_snap"] = True
    elif cmd == "paused":
        state["paused"] = bool(msg.get("paused", False))
        if not state["paused"]:
            state["needs_snap"] = True
    elif cmd == "props":
        props = msg.get("props") or {}
        if state["loaded"]:
            webview.run_javascript(
                f"window.__animaApplyProps && window.__animaApplyProps({json.dumps(props)});",
                None,
                None,
                None,
            )
            state["needs_snap"] = True
    elif cmd == "audio":
        if state["loaded"]:
            arr = msg.get("data") or []
            webview.run_javascript(
                f"window.__animaPushAudio && window.__animaPushAudio({json.dumps(arr)});",
                None,
                None,
                None,
            )
    elif cmd == "mouse":
        if state["loaded"]:
            x = float(msg.get("x", 0.0))
            y = float(msg.get("y", 0.0))
            down = "true" if msg.get("down") else "false"
            down_changed = "true" if msg.get("down_changed") else "false"
            webview.run_javascript(
                f"window.__animaMouseInput && window.__animaMouseInput({x:.4f}, {y:.4f}, {down}, {down_changed});",
                None,
                None,
                None,
            )
    elif cmd == "quit":
        state["closed"] = True
        Gtk.main_quit()
    return False

def stdin_loop():
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
                GLib.idle_add(handle_cmd, msg)
            except Exception:
                pass
    finally:
        state["closed"] = True
        GLib.idle_add(Gtk.main_quit)

threading.Thread(target=stdin_loop, daemon=True).start()
Gtk.main()
"###;

struct WebFrame {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

struct WorkerShared {
    latest_frame: Option<WebFrame>,
    exited: bool,
}

struct WebWorker {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    shared: Arc<(Mutex<WorkerShared>, Condvar)>,
    reader: Option<JoinHandle<()>>,
}

impl WebWorker {
    fn spawn(
        entry: &Path,
        width: i32,
        height: i32,
        initial_props: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self> {
        let abs = entry
            .canonicalize()
            .unwrap_or_else(|_| entry.to_path_buf());
        let file_arg = abs.to_string_lossy().into_owned();
        let init_json = serde_json::Value::Object(initial_props.clone()).to_string();

        let mut child = Command::new("python3")
            .args([
                "-u",
                "-c",
                WEBKIT_WORKER_SCRIPT,
                &file_arg,
                &width.max(64).to_string(),
                &height.max(64).to_string(),
                &init_json,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning offscreen WebKit2GTK worker")?;

        let stdin = BufWriter::new(child.stdin.take().context("taking worker stdin")?);
        let mut stdout = child.stdout.take().context("taking worker stdout")?;

        let shared = Arc::new((
            Mutex::new(WorkerShared {
                latest_frame: None,
                exited: false,
            }),
            Condvar::new(),
        ));
        let shared_rx = Arc::clone(&shared);

        let reader = std::thread::Builder::new()
            .name("anima-webkit-rx".into())
            .spawn(move || {
                let mut hdr = [0u8; 8];
                loop {
                    if stdout.read_exact(&mut hdr).is_err() {
                        break;
                    }
                    let w = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
                    let h = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
                    if w == 0 || h == 0 || w > 16384 || h > 16384 {
                        break;
                    }
                    let Some(len) = (w as usize)
                        .checked_mul(h as usize)
                        .and_then(|px| px.checked_mul(4))
                    else {
                        break;
                    };
                    let mut bgra = vec![0u8; len];
                    if stdout.read_exact(&mut bgra).is_err() {
                        break;
                    }
                    let (lock, cvar) = &*shared_rx;
                    if let Ok(mut guard) = lock.lock() {
                        guard.latest_frame = Some(WebFrame {
                            width: w,
                            height: h,
                            bgra,
                        });
                        cvar.notify_all();
                    }
                }
                let (lock, cvar) = &*shared_rx;
                if let Ok(mut guard) = lock.lock() {
                    guard.exited = true;
                    cvar.notify_all();
                }
            })
            .ok();

        Ok(Self {
            child,
            stdin,
            shared,
            reader,
        })
    }

    fn send_json(&mut self, val: &serde_json::Value) {
        let mut line = val.to_string();
        line.push('\n');
        let _ = self.stdin.write_all(line.as_bytes());
        let _ = self.stdin.flush();
    }

    fn take_latest_frame(&self, wait_first: Option<Duration>) -> Option<WebFrame> {
        let (lock, cvar) = &*self.shared;
        let Ok(mut guard) = lock.lock() else {
            return None;
        };
        if let Some(timeout) = wait_first {
            if guard.latest_frame.is_none() && !guard.exited {
                if let Ok((g, _)) = cvar.wait_timeout_while(guard, timeout, |s| {
                    s.latest_frame.is_none() && !s.exited
                }) {
                    guard = g;
                } else {
                    return None;
                }
            }
        }
        guard.latest_frame.take()
    }
}

impl Drop for WebWorker {
    fn drop(&mut self) {
        self.send_json(&serde_json::json!({ "cmd": "quit" }));
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

pub struct WebRenderer {
    triangle: Triangle,
    program: Program,
    bg_texture: glow::Texture,
    live_texture: glow::Texture,
    live_tex_size: (u32, u32),
    has_live_frame: bool,
    waited_initial: bool,
    spectrum_texture: glow::Texture,
    has_bg: bool,
    tex_size: (u32, u32),
    uv_scale: (f32, f32),
    scheme_color: [f32; 3],
    bcs: [f32; 3],
    hue: f32,
    show_clock: bool,
    show_bars: bool,
    has_audio_listener: bool,
    properties: HashMap<String, String>,
    property_types: HashMap<String, String>,
    worker: Option<WebWorker>,
    last_audio_push: Instant,
    last_mouse_push: Instant,
    last_mouse: [f32; 2],
    last_mouse_down: bool,
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
        let mut show_clock = false;
        let mut show_bars = false;
        let mut properties = HashMap::new();
        let mut property_types = HashMap::new();
        let mut we_props = serde_json::Map::new();

        let html = std::fs::read_to_string(entry).unwrap_or_default();
        let has_audio_listener = detect_audio_listener(root, &html);

        if let Some(proj) = project {
            if let Some(props) = proj
                .get("general")
                .and_then(|g| g.get("properties"))
                .and_then(|p| p.as_object())
            {
                for (k, v) in props {
                    if let Some(ty) = v.get("type").and_then(|t| t.as_str()) {
                        property_types.insert(k.clone(), ty.to_string());
                    }
                    if let Some(val) = v.get("value") {
                        let s = match val {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        if k.eq_ignore_ascii_case("schemecolor") {
                            scheme_color = parse_vec3_str(&s, scheme_color);
                        }
                        properties.insert(k.clone(), s);
                        we_props.insert(k.clone(), serde_json::json!({ "value": val }));
                    }
                }
            }
        }

        let worker = WebWorker::spawn(entry, width, height, &we_props).ok();
        if worker.is_none() {
            let html_lower = html.to_ascii_lowercase();
            show_clock = true;
            show_bars = html_lower.contains("wallpaperregisteraudiolistener")
                || html_lower.contains("audio");
        }

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

        let live_texture = create_texture(gl, 1, 1, &[0, 0, 0, 255], Filter::Linear, false)?;
        let spec_init = vec![0u8; 64 * 4];
        let spectrum_texture = create_texture(gl, 64, 1, &spec_init, Filter::Linear, false)?;

        let mut this = Self {
            triangle,
            program,
            bg_texture,
            live_texture,
            live_tex_size: (1, 1),
            has_live_frame: false,
            waited_initial: false,
            spectrum_texture,
            has_bg,
            tex_size,
            uv_scale: (1.0, 1.0),
            scheme_color,
            bcs: [0.0, 1.0, 1.0],
            hue: 0.0,
            show_clock,
            show_bars,
            has_audio_listener,
            properties,
            property_types,
            worker,
            last_audio_push: Instant::now(),
            last_mouse_push: Instant::now(),
            last_mouse: [-1.0, -1.0],
            last_mouse_down: false,
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

    fn property_json_value(&self, key: &str, value: &str) -> serde_json::Value {
        match self.property_types.get(key).map(String::as_str) {
            Some("bool") => serde_json::Value::Bool(value == "true" || value == "1"),
            Some("slider") => value
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(value.to_string())),
            _ => {
                if value == "true" {
                    serde_json::Value::Bool(true)
                } else if value == "false" {
                    serde_json::Value::Bool(false)
                } else if let Ok(i) = value.parse::<i64>() {
                    serde_json::Value::Number(i.into())
                } else if let Some(n) = value
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                {
                    serde_json::Value::Number(n)
                } else {
                    serde_json::Value::String(value.to_string())
                }
            }
        }
    }
}

impl Renderer for WebRenderer {
    fn render(&mut self, gl: &glow::Context, info: &FrameInfo) -> Result<bool> {
        let wait_first = if !self.has_live_frame && !self.waited_initial {
            self.waited_initial = true;
            Some(Duration::from_millis(400))
        } else {
            None
        };

        if let Some(worker) = self.worker.as_mut() {
            if let Some(frame) = worker.take_latest_frame(wait_first) {
                unsafe {
                    gl.bind_texture(glow::TEXTURE_2D, Some(self.live_texture));
                    if (frame.width, frame.height) != self.live_tex_size {
                        gl.tex_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            glow::RGBA8 as i32,
                            frame.width as i32,
                            frame.height as i32,
                            0,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(&frame.bgra)),
                        );
                        self.live_tex_size = (frame.width, frame.height);
                    } else {
                        gl.tex_sub_image_2d(
                            glow::TEXTURE_2D,
                            0,
                            0,
                            0,
                            frame.width as i32,
                            frame.height as i32,
                            glow::RGBA,
                            glow::UNSIGNED_BYTE,
                            glow::PixelUnpackData::Slice(Some(&frame.bgra)),
                        );
                    }
                    gl.bind_texture(glow::TEXTURE_2D, None);
                }
                self.has_live_frame = true;
            }

            if !self.paused {
                if self.has_audio_listener && self.last_audio_push.elapsed() >= Duration::from_millis(50) {
                    let mut arr = Vec::with_capacity(128);
                    arr.extend_from_slice(&info.spectrum);
                    arr.extend_from_slice(&info.spectrum);
                    worker.send_json(&serde_json::json!({
                        "cmd": "audio",
                        "data": arr,
                    }));
                    self.last_audio_push = Instant::now();
                }
                let down = info.mouse_down;
                let down_changed = down != self.last_mouse_down;
                let moved = (info.mouse[0] - self.last_mouse[0]).abs() > 1.0
                    || (info.mouse[1] - self.last_mouse[1]).abs() > 1.0;
                if down_changed
                    || (moved && self.last_mouse_push.elapsed() >= Duration::from_millis(25))
                {
                    let nx = (info.mouse[0] / self.width.max(1) as f32).clamp(0.0, 1.0);
                    let ny = (info.mouse[1] / self.height.max(1) as f32).clamp(0.0, 1.0);
                    worker.send_json(&serde_json::json!({
                        "cmd": "mouse",
                        "x": nx,
                        "y": ny,
                        "down": down,
                        "down_changed": down_changed,
                    }));
                    self.last_mouse = info.mouse;
                    self.last_mouse_down = down;
                    self.last_mouse_push = Instant::now();
                }
            }
        }

        if !self.has_live_frame || self.show_bars {
            let mut spec_rgba = [0u8; 64 * 4];
            for i in 0..64 {
                let v = (info.spectrum[i].clamp(0.0, 1.0) * 255.0) as u8;
                spec_rgba[i * 4] = v;
                spec_rgba[i * 4 + 1] = v;
                spec_rgba[i * 4 + 2] = v;
                spec_rgba[i * 4 + 3] = 255;
            }
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
            }
        }

        let (hh, mm, ss) = local_hms();

        unsafe {
            gl.viewport(0, 0, self.width, self.height);
            self.program.bind(gl);
            self.program
                .set_i32(gl, "u_web_live", i32::from(self.has_live_frame));
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
            self.program.set_vec4(
                gl,
                "u_bcs_hue",
                [self.bcs[0], self.bcs[1], self.bcs[2], self.hue],
            );

            gl.active_texture(glow::TEXTURE0);
            let active_tex = if self.has_live_frame {
                self.live_texture
            } else {
                self.bg_texture
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(active_tex));
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

        Ok(!self.paused)
    }

    fn resize(&mut self, _gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        self.width = width;
        self.height = height;
        self.update_uv();
        if let Some(worker) = self.worker.as_mut() {
            worker.send_json(&serde_json::json!({
                "cmd": "resize",
                "w": width,
                "h": height,
            }));
        }
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if let Some(worker) = self.worker.as_mut() {
            worker.send_json(&serde_json::json!({
                "cmd": "paused",
                "paused": paused,
            }));
        }
    }

    fn set_volume(&mut self, _volume: f32) {}

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        self.properties.insert(key.to_string(), value.to_string());
        match key.to_ascii_lowercase().as_str() {
            "schemecolor" | "color" => {
                self.scheme_color = parse_vec3_str(value, self.scheme_color);
            }
            "clock" | "show_clock" => {
                self.show_clock = matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                );
            }
            "bars" | "visualizer" | "show_bars" => {
                self.show_bars = matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                );
            }
            "brightness" | "ui_brightness" => {
                if let Ok(v) = value.trim().parse::<f32>() {
                    self.bcs[0] = v.clamp(-1.0, 1.0);
                }
            }
            "contrast" | "ui_contrast" => {
                if let Ok(v) = value.trim().parse::<f32>() {
                    self.bcs[1] = v.clamp(0.0, 3.0);
                }
            }
            "saturation" | "ui_saturation" => {
                if let Ok(v) = value.trim().parse::<f32>() {
                    self.bcs[2] = v.clamp(0.0, 3.0);
                }
            }
            "hue" | "hueshift" | "ui_hue" => {
                if let Ok(v) = value.trim().parse::<f32>() {
                    self.hue = v.clamp(-180.0, 180.0).to_radians();
                }
            }
            _ => {}
        }
        let val = self.property_json_value(key, value);
        if let Some(worker) = self.worker.as_mut() {
            let mut props = serde_json::Map::new();
            props.insert(key.to_string(), serde_json::json!({ "value": val }));
            worker.send_json(&serde_json::json!({
                "cmd": "props",
                "props": props,
            }));
        }
        Ok(())
    }

    fn uses_audio(&self) -> bool {
        self.has_audio_listener || self.show_bars || !self.has_live_frame
    }

    fn describe(&self) -> String {
        format!("web {}", self.entry.display())
    }

    fn destroy(&mut self, gl: &glow::Context) {
        self.worker.take();
        unsafe {
            gl.delete_texture(self.bg_texture);
            gl.delete_texture(self.live_texture);
            gl.delete_texture(self.spectrum_texture);
            gl.delete_vertex_array(self.triangle.vao);
            gl.delete_buffer(self.triangle.vbo);
        }
        self.program.destroy(gl);
    }
}

fn detect_audio_listener(root: &Path, html: &str) -> bool {
    if html
        .to_ascii_lowercase()
        .contains("wallpaperregisteraudiolistener")
    {
        return true;
    }
    let mut stack = vec![root.to_path_buf()];
    let mut checked = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if stack.len() < 16 {
                    stack.push(p);
                }
                continue;
            }
            let is_js = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("js"));
            if !is_js {
                continue;
            }
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if len > 512 * 1024 {
                continue;
            }
            if let Ok(src) = std::fs::read_to_string(&p) {
                if src
                    .to_ascii_lowercase()
                    .contains("wallpaperregisteraudiolistener")
                {
                    return true;
                }
            }
            checked += 1;
            if checked >= 32 {
                return false;
            }
        }
    }
    false
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
