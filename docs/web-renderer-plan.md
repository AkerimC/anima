# Web wallpapers — WebKit renderer

Status: **implemented** in `src/render/web.rs`. This document describes the
renderer as it ships; the earlier design draft (a GTK4 layer-shell helper with
`gtk4-layer-shell`) was replaced by the offscreen process described below
because it needs no compositor-specific protocol work and shares the engine's
existing texture path.

## Goal

Render Wallpaper Engine `web` projects (an `index.html` plus `project.json`,
assets resolved relative to it) as a real browser page while keeping:

- static image / video / shader / scene wallpapers unchanged and lightweight;
- the daemon event loop free of GTK main-loop coupling;
- isolation from crashes and leaks in WebKit;
- a graceful fallback when WebKit or PyGObject is unavailable.

## Architecture

```
animad (Wayland + EGL)                     python3 worker (GTK3 + WebKit2GTK 4.1)
├─ layer-shell background surface           ├─ Gtk.OffscreenWindow + WebKit2.WebView
├─ frames: BGRA over stdout ────────┐       ├─ get_snapshot() loop at web_fps
├─ control: JSON lines over stdin ──┴─────► ├─ JS bridges (properties/audio/pointer)
└─ fallback: procedural shader backdrop      └─ internal scale = web_scale
```

One worker process per web output. The worker keeps WebKit off the daemon's
event loop and confines its RSS; killing the child reclaims everything.

Frame protocol: each frame is an 8-byte little-endian `(width, height)` header
followed by `width * height * 4` bytes of BGRA, streamed over stdout. The
worker duplicates the original stdout to a private file descriptor and points
fd 1 at stderr so GTK/WebKit/Mesa log lines can never corrupt the binary
stream. The daemon keeps one spare frame buffer so a steady 1080p stream does
not allocate (and zero) 8 MB per frame.

The worker serves the wallpaper directory from a loopback HTTP server
(`http://127.0.0.1:<port>/index.html`) instead of `file://`, which keeps
relative asset loads and WebGL textures same-origin. User properties that
point at local files are rewritten to loopback URLs so pages can fetch them.

Control protocol: JSON lines on stdin (`props`, `audio`, `pointer`, `pause`,
`reload`, …). Property updates go through
`wallpaperPropertyListener.applyUserProperties`; general settings (the fps the
daemon snapshots at) go through `applyGeneralProperties`.

## JavaScript bridges

Injected before page scripts run (user content manager, `START`):

- `window.wallpaperPropertyListener` — property updates and
  `applyGeneralProperties` (`fps`).
- `window.wallpaperRegisterAudioListener` — 64-band spectrum + beat pumped at
  the engine's audio rate; audio capture stays idle when the page never
  registers a listener.
- Media integration: `wallpaperMediaIntegration`,
  `wallpaperRegisterMediaStatusListener`, `…PropertiesListener`,
  `…PlaybackListener`, `…ThumbnailListener`, `…TimelineListener`, and
  `wallpaperRequestRandomFileForProperty`.
- Pointer/mouse events are replayed as `pointermove`, `pointerdown`,
  `pointerup` and `mousemove`/`mousedown`/`mouseup` with page-relative
  coordinates, so hover- and click-driven wallpapers work.

WebGL is enabled; WebKit's hardware acceleration policy is `NEVER` because the
worker renders to an offscreen window and snapshots it — the GPU path buys
nothing there and costs a second GL context.

## Pacing and quality knobs

- `web_fps` (default 30, clamped 1..120): snapshot rate. The page is told the
  same number through `applyGeneralProperties`.
- `web_scale` (default 1.0, clamped 0.25..1.0): internal render size. The
  daemon upscales the returned frame; lowering this is the main CPU dial.
- WebKit drops snapshot callbacks occasionally; the worker has a watchdog that
  re-requests the snapshot instead of stalling the stream.
- A worker that exits or times out is reported in `anima status` and
  `anima verify` (`live=true alive=true` per item).

## Dependencies and fallback

Runtime, not build-time:

- `python3` with PyGObject (`python-gobject`)
- GTK 3 and WebKit2GTK 4.1 (`webkit2gtk-4.1` on Arch,
  `gir1.2-webkit2-4.1` on Debian/Ubuntu)

The daemon only needs `python3` on `PATH`. If the worker cannot start or the
`gi` module is missing, the renderer logs one warning and keeps the previous
behaviour: a procedural shader backdrop using the wallpaper's accent colour.

## Security

Pages are local Workshop files but run with browser privileges. The worker
serves them from a loopback HTTP server bound to `127.0.0.1` so assets and
WebGL textures load same-origin; pages are trusted input — the same policy the
engine applies to shaders and scenes (see the README limitations). Do not
install Workshop web wallpapers from untrusted sources.

## Verification

`anima verify` spawns the real worker and requires a live frame before the
timeout, so it covers the whole chain: GTK init, WebKit load, snapshot stream
and texture upload. The eight web wallpapers in the reference library verify
with `live=true alive=true`; a missing WebKit install turns them into `WARN`
items instead of failures.

## Open items

- **Software rasterisation.** WebKit renders these pages on the CPU, so
  WebGL-heavy wallpapers are limited by `web_fps` × `web_scale`, not by the
  engine's GPU budget.
- **No GPU texture sharing.** Frames cross the process boundary as pixels.
  A DMA-BUF path would remove the copy but adds driver-specific complexity.
- **One process per output.** Fine for the handful of monitors the daemon
  targets; a shared browser process would need per-page surfaces.
