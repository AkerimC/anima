# Compositors

Drawing (layer-shell, EGL), the characters, the 3D models and the input
devices (`pointer.rs`, evdev) are the same everywhere. Everything that
depends on the compositor is behind the `Compositor` trait in
`src/backend/compositor.rs`. `compositor::current()` detects one
implementation at startup and the rest of animad only talks to that.

| Implementation | File | Detected by |
|---|---|---|
| niri | `backend/niri.rs`; its code still lives in `governor.rs` and is meant to move | `$NIRI_SOCKET` |
| Hyprland | `backend/hyprland.rs` (+ `contrib/hyprland-plugin`) | `$HYPRLAND_INSTANCE_SIGNATURE` socket |
| generic | `Fallback` in `compositor.rs`: window state every 2 s, fullscreen on sway | anything else |

## The trait

Every method except these three has a default that means "not supported":

- `run(tx)`: runs on the governor thread for the life of animad. It watches
  the compositor and sends `GovernorEvent`s (fullscreen/maximized state,
  active window, focused monitor, cursor, power ticks). Start
  `pointer::spawn` from here if the compositor wants clicks, drags and
  gestures, passing `exact = true` if you'll feed the real cursor position
  through `pointer::SYNC`.
- `window_state()`: the same state, asked directly (at startup and by
  `governor::poll_forever`).
- `name()`

The rest:

- `strip()`: what the characters walk along and where it's drawn right now
  (logical px). niri: the scrolling strip of columns. Hyprland: the row of
  workspaces. None: just the screen.
- `cursor()`: for compositors that can be asked where the pointer is
  (patched niri). `pointer.rs` then polls it instead of estimating.
- `swipe(active)`, `swipe_scroll(dx)`: three/four-finger touchpad swipes,
  read from evdev by `pointer.rs`, for compositors that report neither the
  gesture nor the scroll. dx is already through libinput's acceleration
  curve.
- `can_place_windows()`, `set_window_mode(on)`, `row()`,
  `place_workspace_window(slot)`: window mode, see `docs/character-windows.md`.

## Adding a compositor

1. Write `backend/<name>.rs` with a struct, a `detect()` that returns
   `Some` when animad runs on it, and `impl Compositor`.
2. Add it to the detection order in `compositor::current()`.
3. Implement `run` and `window_state` first; everything else is optional
   and can come later.
