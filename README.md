# Anima

A lightweight, native live wallpaper engine for Linux (Wayland), combining
Anima-style desktop characters with Wallpaper Engine style wallpapers.

Written in Rust, rendering directly on the `wlr-layer-shell` background layer
with EGL / OpenGL ES 3.0. No Electron, no Chromium, no XWayland.

```
┌──────────────────────────────┐
│ animad   background daemon   │  layer-shell surface per output
│ anima    CLI + TUI client    │  line-delimited JSON over a Unix socket
└──────────────────────────────┘
```

## Features

| Area | Status |
| --- | --- |
| Static images (png/jpg/webp/bmp/avif/tga/tex) | ✅ native |
| Animated GIF | ✅ native |
| Video (mp4/webm/mkv/mov/avi/m4v/ogv) | ✅ libmpv + hardware decode |
| GLSL shader wallpapers (`.frag`) | ✅ native |
| Scene wallpapers (`scene.toml`, Wallpaper Engine `.pkg`) | ✅ native renderer: layer graph, effects, puppet skeletons, particles, SceneScript, cameras |
| Web wallpapers (HTML/CSS/JS/WebGL) | ✅ offscreen WebKit2GTK process (optional runtime deps, see [Limitations](#limitations)) |
| Headless regression checks (`anima verify`) | ✅ offscreen EGL sweep: shaders, scripts, GL errors, memory |
| Desktop character / mascot | ✅ procedural presets + sprite images |
| Audio-reactive wallpapers & characters | ✅ PulseAudio/PipeWire monitor + FFT |
| Lyrics / viseme lip-sync | ✅ MPRIS + LRCLIB |
| Playlists, next/prev, shuffle | ✅ |
| Multi-monitor, per-output config | ✅ |
| Pause on fullscreen / maximized / battery | ✅ (Hyprland + niri events; polled elsewhere) |
| Steam Workshop browsing + `.pkg` / `.tex` unpack | ✅ |
| Transitions between wallpapers | ❌ not yet (roadmap) |

## Performance model

The engine is built around "do nothing when nothing changes":

- **Static images idle at 0 fps.** A static wallpaper with no character
  overlay costs ~0.5 % CPU and no continuous GPU work.
- **Character over a static wallpaper** runs on its own clock (`character.fps`,
  default 30) and skips redrawing the unchanged wallpaper surface entirely.
  Measured on a 1920×1080 output (AMD Renoir iGPU): ~2.3 % CPU in release.
- **Animated wallpapers** run at the configured `fps` (60 by default), paced by
  compositor frame callbacks — no busy loops.
- **Audio capture sleeps** (0 % CPU, stream closed) unless the active wallpaper
  or character is audio-reactive.
- **Fullscreen / maximized / battery** pause is event-driven, not polled. The
  character overlay is hidden while a fullscreen window is focused.
- **Switching wallpapers reclaims GPU resources.** Render targets, programs,
  textures, meshes and VAOs are deleted explicitly while the EGL context is
  current, so RAM and GTT stay flat across repeated scene/video/web switches.


## Requirements

- Wayland compositor with `wlr-layer-shell` (Hyprland, niri, Sway, …)
- EGL + OpenGL ES 3.0 driver (Mesa)
- `libmpv` (for video wallpapers)
- PulseAudio or PipeWire (for audio-reactive content; optional at build/run time)
- Rust 1.85+ (edition 2024) to build

Optional, for HTML/CSS/JS web wallpapers:

- `python3` with PyGObject (`python-gobject`)
- GTK 3 and WebKit2GTK 4.1 (`webkit2gtk-4.1`)

Without those the daemon still runs; web wallpapers fall back to a procedural
shader backdrop and log a single warning.

## Build

```sh
git clone <repo> anima && cd anima
cargo build --release
```

Binaries:

- `target/release/animad` — the daemon
- `target/release/anima` — the CLI
- `target/release/anima-gui` — the GTK4/libadwaita settings app (optional):

  ```sh
  cargo build --release -p anima-gui
  ```

  Needs the GTK4 and libadwaita development packages. A desktop entry is in
  [`contrib/anima.desktop`](contrib/anima.desktop).

## Run

```sh
# start the daemon (foreground; use the systemd unit for daily use)
./target/release/animad --verbose

# in another terminal
./target/release/anima status
./target/release/anima set ~/Pictures/Wallpapers/fantasy-mountain.jpg
./target/release/anima set examples/wallpapers/cyber_scene
./target/release/anima next
```

### systemd user service

A unit is provided in [`contrib/animad.service`](contrib/animad.service).
It uses `Restart=always` so the daemon recovers from compositor hotplug hiccups.

```sh
mkdir -p ~/.config/systemd/user
cp contrib/animad.service ~/.config/systemd/user/
# edit ExecStart if the checkout is not at ~/Projects/anima
systemctl --user daemon-reload
systemctl --user enable --now animad.service
```

Hyprland users: make sure the session environment is visible to systemd:

```sh
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR \
  XDG_CURRENT_DESKTOP HYPRLAND_INSTANCE_SIGNATURE
```

niri users: pass `NIRI_SOCKET` instead (or as well, if you switch sessions):

```sh
systemctl --user import-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR \
  XDG_CURRENT_DESKTOP NIRI_SOCKET
```

The unit already declares all of these in `PassEnvironment`, so importing
them once per session is enough.

## Compositors

animad detects the compositor at startup and picks the matching backend:
niri if `$NIRI_SOCKET` is set, else Hyprland if
`$HYPRLAND_INSTANCE_SIGNATURE` is set, else a generic polling fallback. The
wallpaper, characters, games and races work the same on both; only the
compositor-specific bits differ.

To force one (both socket variables present, or you keep the Hyprland path
in a nested session):

```toml
[general]
compositor = "auto"      # auto | hyprland | niri | generic
```

or `Environment=ANIMA_COMPOSITOR=hyprland` in the systemd unit. The value is
read once, so restart animad after changing it. A forced backend that isn't
running falls back to detection with a warning.

### niri

- Characters walk along the scrolling strip of columns and follow it during
  swipes. Without a patch niri doesn't report tiled window positions or the
  cursor, so animad estimates them; clicks, drags and gestures come from
  evdev. Install the device rules once:

  ```sh
  sudo cp contrib/70-anima-pointer.rules /etc/udev/rules.d/
  sudo udevadm control --reload && sudo udevadm trigger
  ```

  and log out/in. Without them the wallpaper still renders; only character
  interaction is unavailable.
- For exact positions, apply `contrib/niri-ipc-tiled-window-positions.patch`
  to niri (GPL-3.0-or-later, optional). animad detects the extra IPC fields
  at runtime and uses them; nothing applies the patch automatically.

### Hyprland

- Window mode (one character window per workspace) needs the plugin under
  `contrib/hyprland-plugin`; without it the overlay fallback is used. See
  its README for the window rules.
- The evdev rules above are also what give Hyprland character clicks and
  drags; the cursor position itself comes from Hyprland's IPC.

## CLI

```
anima set [PATH|ID] [-m MONITOR]   set a wallpaper (file, folder, name, Workshop ID)
anima next / prev                  playlist / directory navigation
anima pause / resume / toggle      freeze all or one output (-m)
anima volume [0..1|0..100] [--mute true|false]  video and scene audio
anima prop [key] [value]           list / get / set user properties
anima playlist --interval N --shuffle true
anima char [preset|file|off] [--corner ...] [--placement ...]
anima workshop [QUERY]             scan Steam Workshop items (AppID 431960)
anima verify [--kind K] [--dir P]  headless offscreen regression sweep
anima unpack <file.pkg|file.tex> [out]
anima tui                          interactive control center
anima reload | status | monitors | list
```

`status --json` exposes machine-readable output, including per-monitor frames,
fps, pause state, audio levels and lyric state.

## Configuration

`~/.config/anima/config.toml` (created on first run):

```toml
[general]
fps = 60                    # animated wallpaper frame rate
mute = true                 # mute video and scene audio
volume = 0.0                # video and scene audio volume
automute = true             # mute wallpaper audio when another app plays audio
audio_capture = true        # desktop spectrum/beat analysis
pause_on_fullscreen = true
pause_on_maximized = false
pause_on_battery = false
battery_fps = 30
low_battery_pause = 15      # pause under this %, 0 disables
hwdec = "auto-safe"         # mpv hwdec mode
web_fps = 30                # snapshot rate for HTML/WebGL wallpapers
web_scale = 1.0             # internal WebKit render scale (0.25..1.0)
compositor = "auto"         # auto | hyprland | niri | generic
playlist_interval_secs = 0
playlist_shuffle = false
playlist = []               # explicit entries; empty = wallpapers_dir

[character]
enabled = true
source = "reze"             # reze|neko|cyber|spirit, a sprite or a .glb model path
corner = "bottom-right"
placement = "window-inside" # wallpaper|screen-front|window-inside|window-outside
scale = 1.15
offset = [24.0, 0.0]
opacity = 1.0
flip_x = false
look_at_mouse = true
audio_reactive = true
always_on_top = true
fps = 30                    # character clock over a static wallpaper

[[monitor]]
name = "eDP-1"
wallpaper = "/path/to/wallpaper"
fps = 60                    # optional per-output override

[monitor.properties]
schemecolor = "0.22 0.68 0.98"
```

Runtime changes via the CLI are persisted back to this file.

### 3D characters

`source` (and `extra`) also take a rigged glTF model (`.glb` / `.gltf`). It
plays its idle clip when standing, a walk clip while walking, a reaction clip
when petted and a pick-up clip while dragged (`Cafe_Idle`, `Cafe_Walk`,
`Cafe_Reaction`, `Formation_Pickup`, else any clip named like idle / walk /
react / pickup), turned toward the walking direction. The Blue Archive chibis
from [lihaohong6/BlueArchiveModels](https://github.com/lihaohong6/BlueArchiveModels)
work as-is:

```sh
name="Airi"; dir=~/.local/share/anima/ba/$name; mkdir -p "$dir"
curl -L -o "$dir/$name.glb" \
  "https://raw.githubusercontent.com/lihaohong6/BlueArchiveModels/main/${name// /%20}.glb"
```

then `source = "~/.local/share/anima/ba/Airi/Airi.glb"`, or pick it in
`anima-gui`, which lists `~/.local/share/anima/ba/*/` next to the sprites.

## Wallpaper formats

- **Images** — any format supported by the `image` crate; `.tex` (Wallpaper
  Engine texture) is decoded natively. Static images idle completely.
- **Video** — decoded with libmpv through its OpenGL render API. Hardware
  decoding uses `hwdec=auto-safe`; Vulkan-copy / VAAPI depending on the driver.
- **Shaders** — a single fragment shader source; the engine supplies standard
  uniforms (resolution, time, mouse, spectrum, beat). Multi-pass shaders are not
  supported yet.
- **Scenes** — `scene.toml` with layers and particle presets (`stars`,
  `fireflies`, `embers`, `sakura`, …), parallax and audio reactivity.
  Wallpaper Engine `scene.pkg` archives are supported: model/material
  resolution, texture decoding (RGBA, DXT1/3/5, LZ4), the full layered scene
  graph (element order is draw order), parallax, blend modes, text/clock/date
  layers, skeletal puppet meshes (`MDLV`/`MDLS`/`MDLA` animation layers),
  per-layer effect stacks, and bundled `sound` layers (played through libmpv,
  following the global mute/volume settings).

  SceneScripts run in an embedded QuickJS VM and drive layer visibility,
  alpha, origin/scale/angles, color and text, effect visibility and shader
  constants, puppet animation layers, particle `instanceoverride` values,
  texture/video playback, scene layer creation/destruction/sorting, media
  events and the cursor. Scripts get the standard prelude (`Vec2/3/4`,
  `WEMath`, `WEColor`, `engine`, `shared`, `localStorage` with persistence,
  `thisScene`/`thisLayer`/`thisObject`, audio buffers) and `scriptProperties`
  overrides from the user properties. `createAnimation()` and
  `getTextureAnimation()`/`getVideoTexture()` are wired to real playback.
  A script that fails to compile or throws is disabled and logged; it never
  takes the scene down.

  Cameras are supported: path animations from `camera`/`camerapath`, shake and
  zoom, plus 2.5D perspective division for layers that author it through
  world Z. Scene layers can also act as 2D lights: `light` objects feed the
  layer shader (`u_light_*`, normal maps, `LIGHTING=1`) and the bloom pass
  handles SDR/HDR glow. Composition targets (`composelayer`,
  `projectlayer`, `copybackground`) render through offscreen framebuffers when
  authored with a bounded size.

  Puppet meshes use the skeleton's assembled reference pose (the extra MDLS
  matrix array) or, when absent, the first keyframe of the first clip as the
  neutral pose; animation layers are applied as relative motion tracks on top
  of it. This keeps modelled parts that are stored in an exploded texture-sheet
  layout assembled on screen.

  Wallpaper Engine effects are shipped as HLSL-flavoured GLSL. The engine
  translates them at load time (preprocessor, combo defines, implicit
  int/float and vector-width coercion, global initializer hoisting, builtin
  redefinitions, `%`/array-index rules, return-value truncation) rather than
  patching individual shaders, so community effects keep working as new
  wallpapers are downloaded. Multi-pass effects, dynamic shader constants,
  `usertextures` and `$mediaThumbnail` are supported. The local 91-item
  Workshop library currently verifies with zero shader and zero GL failures.

- **Web** — real HTML/CSS/JS/WebGL pages rendered by WebKit2GTK in an
  offscreen process (see [the web renderer notes](docs/web-renderer-plan.md)).
  The daemon streams frames from it into a GL texture, forwards user
  properties through the standard `wallpaperPropertyListener` bridge, feeds
  the 64-band audio spectrum to `wallpaperRegisterAudioListener`, and replays
  pointer/mouse events into the page. Snapshot rate and internal resolution
  are configurable (`web_fps`, `web_scale`); WebKit rasterises in software, so
  those two values are the CPU dial. If WebKit or PyGObject is missing the
  wallpaper degrades to a procedural shader backdrop instead of failing.

## Verification

`anima verify` renders wallpapers through a headless EGL pbuffer — no
compositor and no visible window — and reports what went wrong per item:

```sh
anima verify                                  # whole Workshop library
anima verify --kind scene                     # one wallpaper type
anima verify --filter 3516106265              # Workshop ID or title substring
anima verify --dir examples/wallpapers/cyber_scene --dump-dir /tmp/frames
anima verify --json --rss-limit-mb 600        # machine-readable + memory ceiling
```

Each item reports shader compile failures, SceneScript failures, GL errors,
rendered coverage and RSS high-water marks; web wallpapers are checked for
live WebKit frames and video wallpapers for decoded frames. The command exits
non-zero only when an item actually fails (`WARN` covers author-broken scripts
and missing optional runtimes). Defaults are 640×360 and 4 frames per scene.

## Limitations

These are deliberate, documented gaps rather than bugs:

1. **Web wallpapers need optional runtime packages.** They are real
   HTML/CSS/JS/WebGL pages rendered by WebKit2GTK 4.1 through the system
   `python3` + PyGObject, not an embedded browser engine. WebKit rasterises in
   software (`web_fps` / `web_scale` are the CPU dial), so heavy WebGL pages
   are CPU-bound rather than GPU-bound. Without WebKitGTK the wallpaper falls
   back to a procedural shader backdrop. See
   [`docs/web-renderer-plan.md`](docs/web-renderer-plan.md).
2. **No wallpaper transitions.** Switching applies immediately.
3. **Wayland only.** X11 and Windows/macOS are out of scope.
4. **Compositor coverage is Hyprland + niri.** Fullscreen/maximized pause,
   the character strip and window mode use compositor IPC; on other
   compositors the wallpaper still renders, but the strip is just the screen,
   fullscreen pause is only detected on sway, and character interaction
   depends on evdev only.
5. **Wallpaper Engine compatibility is broad but not total.** 2D scenes,
   video, web and effect-heavy wallpapers work — including skeletal puppet
   (`.mdl`) animations, bounded `composelayer` / `projectlayer` /
   `copybackground` composition, SceneScript (layer properties, effect
   visibility and constants, puppet layers, particle instance overrides,
   `createAnimation`, audio buffers, media and cursor events), camera paths,
   2.5D perspective layers, 2D lighting and bloom. Still missing:
   `application` wallpapers (Windows `.exe`), and full 3D scene wallpapers that
   build a real mesh/camera 3D world (layers with authored perspective get
   2.5D Z division, not a 3D pipeline). Scripts that are broken at the source
   are reported by `anima verify` and in the log; they are not fixed for you.

6. **Audio analysis is PulseAudio/PipeWire only** (loaded dynamically; the
   daemon still runs without it).
7. **No per-wallpaper sandboxing.** Shaders, scenes and web pages are trusted
   input.

## Roadmap

- [ ] Cross-fade / slide transitions between wallpapers
- [ ] Full 3D Wallpaper Engine scenes (mesh + camera pipeline)
- [ ] `application` wallpapers (out of scope on Linux without Wine-style shims)
- [ ] Multi-pass shader wallpapers (`.json` shader pipelines)
- [ ] Scene editor / hot-reload while editing
- [ ] Wayland `ext-session-lock` integration (pause while locked)
- [ ] Per-output power profiles and HDR output

## Project layout

```
src/
  backend/wayland.rs   layer-shell surfaces, EGL, pacing, IPC, playback
  backend/compositor.rs  compositor trait + auto-detection
  backend/hyprland.rs  Hyprland IPC, workspace strip, plugin placement
  backend/niri.rs      niri backend entry (implementation in governor.rs)
  backend/governor.rs  compositor events, niri strip and gestures, power state
  render/              image, video, shader, scene, scenescript, particles,
                       effects, web, character, gl helpers
  audio.rs             PulseAudio monitor capture + FFT + beat detection
  media.rs             MPRIS2 session/media events and album art
  lyrics.rs            MPRIS + LRCLIB lyrics and viseme sequencer
  pkg.rs               Wallpaper Engine .pkg / .tex reader
  wallpaper.rs         wallpaper discovery and metadata
  config.rs            TOML configuration
  ipc.rs               line-delimited JSON protocol
  verify.rs            headless EGL regression runner
  bin/animad.rs        daemon entry point
  bin/anima.rs         CLI / TUI
examples/wallpapers/   particle scene, clock HUD web project
docs/                  renderer, compositor and web design notes
```

## License

MIT — see [LICENSE](LICENSE).
