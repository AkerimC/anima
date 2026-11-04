# Real web wallpapers — design plan

Status: **planned, not implemented.** The current `src/render/web.rs` is a
metadata-driven GPU HUD, not a browser. This document describes how a real
HTML/CSS/JS renderer would be added without compromising the daemon's
low-overhead event loop.

## Goal

Render Wallpaper Engine style `web` projects (an `index.html` plus
`project.json`, optionally referenced assets) as an actual browser page on the
background layer, while keeping:

- static image / video / shader / scene wallpapers unchanged and lightweight;
- the daemon event loop free of GTK main-loop coupling;
- isolation from crashes and memory leaks in WebKit;
- a graceful fallback to the current HUD renderer when WebKit or
  `gtk4-layer-shell` is unavailable.

## Why not embed WebKit in the daemon process

WebKitGTK requires a running `GMainContext`, owns its own Wayland/EGL surfaces
and utilizes several helper processes. Embedding it in `animad` would:

- force GTK initialization into the Wayland event loop thread;
- make WebKit crashes fatal for every wallpaper;
- pin 200–400 MB of RSS to the daemon even after switching to an image;
- complicate the existing EGL context / frame-callback pacing model.

**Decision: a separate helper process**, `anima-webhelper`, spawned per web
output and terminated when it is no longer needed.

```
animad (Wayland + EGL)                     anima-webhelper (GTK4 + WebKitGTK)
├─ layer-shell background surface   ←──┐   ├─ GTK4 layer-shell window
├─ IPC: anima.sock                     │   │   (Layer::Background, anchors all)
└─ spawn/kill + JSON control socket ──┴──►  ├─ WebKitWebView (index.html)
                                             └─ JS bridges (audio/properties)
```

The helper owns the layer surface for that output; `animad` does not create its
own background surface while a web wallpaper is active on that monitor.

## Dependencies (optional feature)

Add a Cargo feature `web` (off by default) pulling:

| Crate | Purpose |
| --- | --- |
| `gtk4` | GTK4 bindings |
| `webkit6` | WebKitGTK 6.0 bindings (`webkitgtk-6.0.pc`) |
| `gtk4-layer-shell` | layer-shell integration for GTK4 windows |

System packages: `gtk4`, `webkitgtk-6.0`, `gtk4-layer-shell` (available in the
Arch `extra` repository). The helper binary is only built with
`--features web`; a default build remains dependency-light.

Runtime detection: `animad` checks for the helper next to its own executable
and for `libwebkitgtk-6.0.so`; if either is missing, it keeps using the HUD
renderer and logs a single warning.

## Helper responsibilities

1. Parse `project.json` (`type`, `file`, `general.properties`) — reuse
   `crate::wallpaper` metadata where possible.
2. Create a GTK4 window and turn it into a background layer surface:
   - `Layer::Background`, anchors on all edges, `exclusive_zone(-1)`,
     keyboard interactivity `None`, empty input region (clicks pass through).
3. Load `index.html` from the project root via `file://`; deny navigation to
   anything outside that root; disable devtools and persistent storage.
4. WebKit settings: hardware acceleration on, media playback disabled unless
   the project asks for it, background throttling relaxed.
5. Report readiness and failures on its control socket so the daemon can fall
   back to the HUD renderer.

## JS bridge

Wallpaper Engine pages expect two global hooks. The helper injects a user
script before page load and drives them from the daemon:

- `window.wallpaperRegisterAudioListener(cb)` — called at ~30 Hz with
  `Uint8Array(128)` (left/right channel pairs) derived from
  `AudioSnapshot.spectrum`. The helper requests audio frames from `animad`
  over its control socket, so the existing FFT/beat analyzer remains the single
  source of truth and sleeps when nothing needs audio.
- `window.wallpaperPropertyListener` — `applyUserProperties({...})` on load and
  on every `anima prop set`, with Wallpaper Engine value shapes
  (`{"value": ...}` for sliders/bools, `"r g b"` for colors).
- Optional: `applyGeneralProperties` for fps, and mouse position via
  `mousemove` events synthesized from `animad`'s cursor tracking.

## Lifecycle and pacing

- `animad` spawns the helper when a web wallpaper is assigned to an output and
  kills it on wallpaper change, output removal, or shutdown.
- Pause integration: on fullscreen / maximized / battery / user pause the
  daemon sends `suspend`; the helper calls
  `webkit_web_view_set_is_web_process_suspended(true)` and mutes the view.
  Resume reverses this. This keeps a paused web wallpaper near 0 % CPU.
- WebKit drives its own repaint clock; the daemon does not arm frame callbacks
  for that output. FPS is therefore controlled by
  `webkit_settings_set_hardware_acceleration_policy` and page `requestAnimationFrame`,
  not by the engine's `fps` field. Document this explicitly in `anima status`
  (`kind: web-browser`).
- Idle static pages still repaint only when the page schedules work; no
  engine-side timer is needed.

## Failure handling

| Failure | Behaviour |
| --- | --- |
| helper binary missing | HUD fallback, one warning |
| `gtk4-layer-shell` missing | helper exits with a clear error, HUD fallback |
| WebView load error | helper reports URL + error, HUD fallback |
| helper crash | daemon restarts it up to 3 times, then HUD fallback |
| output hotplug | helper bound to one output; daemon respawns on the new output |

## Milestones

1. **PoC** — `--features web` helper that opens a GTK4 layer-shell window with
   a WebView on one output and renders a local test page. Measure RSS/CPU.
2. **Supervision** — spawn/kill from `animad`, control socket, readiness and
   crash/restart handling, HUD fallback.
3. **Property bridge** — `anima prop` reaches the page; defaults from
   `project.json`.
4. **Audio bridge** — 64-band spectrum + beat pumped at ~30 Hz; audio capture
   sleeps when no web or audio-reactive wallpaper is active.
5. **Pause/power** — fullscreen, maximized, battery and user pause suspend the
   web process; overlay character still works above it.
6. **Multi-monitor + polish** — per-output lifecycle, `status` reporting,
   documentation, packaging notes for Arch/Fedora.

## Risks / open questions

- **Memory:** WebKit spawns Web/Network/GPU processes; expect 200–400 MB per
  web wallpaper. Only acceptable because web wallpapers are an explicit opt-in.
- **GPU sharing:** WebKitGTK renders through its own EGL/GBM path. No texture
  sharing with the engine's GL context is planned; the compositor blends the
  layer surface like any other window. Verify on AMD iGPU and NVIDIA.
- **Pacing:** WebKit ignores the engine fps cap. If needed, throttle with a
  page-injected `requestAnimationFrame` limiter.
- **Security:** pages are local files but still run with browser privileges.
  Disable remote navigation, devtools, storage and downloads; document that
  Workshop web wallpapers are trusted content.
- **Packaging:** the optional feature must not break distro builds without
  WebKitGTK; CI should build both feature sets.
