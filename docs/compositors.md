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

## Choosing one

By default animad detects the compositor (`auto`: niri, then Hyprland, then
generic). Either backend can be forced, which is useful if both sockets are
in the environment, or to keep the Hyprland path on a nested/niri session:

```toml
[general]
compositor = "niri"      # auto | hyprland | niri | generic
```

`ANIMA_COMPOSITOR` overrides the config file, so a systemd unit can pin it
with `Environment=ANIMA_COMPOSITOR=hyprland`. A forced backend that isn't
running logs a warning and falls back to detection; the daemon still starts.
The value is read once at startup, so restart animad after changing it
(`systemctl --user restart animad`).

Both backends use the same character, game and race code. The differences
are what the compositor can tell us and how windows can be placed:

| | Hyprland | niri |
|---|---|---|
| fullscreen/maximized/active window | IPC events | IPC events |
| strip (`strip()`) | row of workspaces (`hypr_view`) | scrolling columns, exact with the optional patch |
| cursor | `cursorpos` over IPC | estimated from evdev (exact with the optional patch) |
| window mode (`can_place_windows`) | with `contrib/hyprland-plugin` | not implemented; overlay is used |
| game/race window (`columns()`) | whole screen (one workspace) | tiled columns on the active workspace |

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
- `columns()`: tiled windows on the active workspace, `(x, width)` on the
  strip, for games and races that want a wide window. niri reports its
  columns; Hyprland leaves it empty and the whole screen is used.
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
2. Add it to the detection order in `compositor::current()` (and to the
   `compositor` values in `src/config.rs` if users should be able to force it).
3. Implement `run` and `window_state` first; everything else is optional
   and can come later.
