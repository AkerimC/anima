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
| Scene wallpapers (`scene.toml`, Wallpaper Engine `.pkg`) | ✅ native renderer: image/text/particle layers, layer effects, puppet skeletons |
| Web wallpapers (HTML/CSS/JS) | ⚠️ approximated — see [Limitations](#limitations) |
| Desktop character / mascot | ✅ procedural presets + sprite images |
| Audio-reactive wallpapers & characters | ✅ PulseAudio/PipeWire monitor + FFT |
| Lyrics / viseme lip-sync | ✅ MPRIS + LRCLIB |
| Playlists, next/prev, shuffle | ✅ |
| Multi-monitor, per-output config | ✅ |
| Pause on fullscreen / maximized / battery | ✅ (Hyprland + niri events; polled elsewhere) |
| Steam Workshop browsing + `.pkg` / `.tex` unpack | ✅ |
| Transitions between wallpapers | ❌ not yet (roadmap) |
| Real HTML/JS web wallpapers | ❌ not yet (roadmap) |

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
anima volume [0..1|0..100] [--mute true|false]  video audio
anima prop [key] [value]           list / get / set user properties
anima playlist --interval N --shuffle true
anima char [preset|file|off] [--corner ...] [--placement ...]
anima workshop [QUERY]             scan Steam Workshop items (AppID 431960)
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
mute = true
volume = 0.0
automute = true             # mute video when another app plays audio
audio_capture = true        # desktop spectrum/beat analysis
pause_on_fullscreen = true
pause_on_maximized = false
pause_on_battery = false
battery_fps = 30
low_battery_pause = 15      # pause under this %, 0 disables
hwdec = "auto-safe"         # mpv hwdec mode
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
  layers, skeletal puppet meshes (`MDLV`/`MDLS`/`MDLA` animation layers) and
  per-layer effect stacks.

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
  wallpapers are downloaded. The bundled 19-scene Workshop test library
  currently compiles with zero shader failures.

- **Web** — metadata-driven HUD (see below).

## Limitations

These are deliberate, documented gaps rather than bugs:

1. **Web wallpapers are not a real browser.** `src/render/web.rs` reads
   `project.json` / `index.html` metadata and synthesizes a GPU HUD (clock,
   spectrum bars, accent color). Arbitrary HTML/CSS/JS is **not** executed.
   A WebKitGTK-based helper process is planned; see
   [`docs/web-renderer-plan.md`](docs/web-renderer-plan.md).
2. **No wallpaper transitions.** Switching applies immediately.
3. **Wayland only.** X11 and Windows/macOS are out of scope.
4. **Compositor coverage is Hyprland + niri.** Fullscreen/maximized pause,
   the character strip and window mode use compositor IPC; on other
   compositors the wallpaper still renders, but the strip is just the screen,
   fullscreen pause is only detected on sway, and character interaction
   depends on evdev only.
5. **Wallpaper Engine compatibility is partial.** Common 2D scene, video and
   effect-heavy wallpapers work. Not yet implemented: scene render-target
   composition (`_rt_imageLayerComposite_*` / `copybackground` instanced solid
   layers such as day-night composites — these are skipped instead of being
   drawn as white rectangles), 3D/perspective meshes, Wallpaper Engine's native
   particle systems beyond the generic emitter model, and `application`
   wallpapers. Custom effect math may still differ visually from the original.

6. **Audio analysis is PulseAudio/PipeWire only** (loaded dynamically; the
   daemon still runs without it).
7. **No per-wallpaper sandboxing.** Shaders and scenes are trusted input.

## Roadmap

- [ ] Real HTML/CSS/JS wallpapers via a WebKitGTK helper process
      ([plan](docs/web-renderer-plan.md))
- [ ] Cross-fade / slide transitions between wallpapers
- [ ] Full Wallpaper Engine `.pkg` scene graph coverage
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
  render/              image, video, shader, scene, web, character, gl helpers
  audio.rs             PulseAudio monitor capture + FFT + beat detection
  lyrics.rs            MPRIS + LRCLIB lyrics and viseme sequencer
  pkg.rs               Wallpaper Engine .pkg / .tex reader
  wallpaper.rs         wallpaper discovery and metadata
  config.rs            TOML configuration
  ipc.rs               line-delimited JSON protocol
  bin/animad.rs        daemon entry point
  bin/anima.rs         CLI / TUI
examples/wallpapers/   aurora shader, particle scene, clock HUD project
docs/                  design notes (web renderer plan)
```

## License

MIT — see [LICENSE](LICENSE).
