//! Wallpaper Engine keyframe animation tracks (`c0`..`c3`) with cubic
//! Bézier handles and script-controllable playback state (`startpaused`,
//! `play`, `pause`, `stop`, `setFrame`, `rate`).

/// Playback loop behaviour from `animation.options.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackMode {
    Loop,
    Mirror,
    Single,
}

impl TrackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TrackMode::Loop => "loop",
            TrackMode::Mirror => "mirror",
            TrackMode::Single => "single",
        }
    }
}

/// Incoming (`back`) or outgoing (`front`) cubic Bézier handle on a keyframe.
#[derive(Debug, Clone, Copy)]
pub struct KeyHandle {
    pub enabled: bool,
    pub x: f32,
    pub y: f32,
}

/// A single keyframe sample on one scalar curve (`c0`..`c3`).
#[derive(Debug, Clone, Copy)]
pub struct Keyframe {
    pub frame: f32,
    pub value: f32,
    pub back: KeyHandle,
    pub front: KeyHandle,
}

/// Animated multi-component keyframe track parsed from Wallpaper Engine
/// `"animation": { "c0": [...], "options": { ... } }` objects.
#[derive(Debug, Clone)]
pub struct KeyTrack {
    pub name: String,
    pub fps: f32,
    pub length: f32,
    pub mode: TrackMode,
    pub relative: bool,
    pub start_paused: bool,
    pub wrap_loop: bool,
    pub curves: Vec<Vec<Keyframe>>,
    pub playing: bool,
    pub rate: f32,
    pub frame: f32,
    pub mirror_reverse: bool,
}

fn json_f32(v: &serde_json::Value) -> Option<f32> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::Object(o) => o.get("value").and_then(json_f32),
        _ => None,
    }
}

fn json_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => Some(n.as_f64().unwrap_or(0.0) != 0.0),
        serde_json::Value::String(s) => {
            Some(s.eq_ignore_ascii_case("true") || s == "1" || s.eq_ignore_ascii_case("on"))
        }
        serde_json::Value::Object(o) => o.get("value").and_then(json_bool),
        _ => None,
    }
}

fn parse_handle(v: Option<&serde_json::Value>, default_x: f32) -> KeyHandle {
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return KeyHandle {
            enabled: false,
            x: default_x,
            y: 0.0,
        };
    };
    KeyHandle {
        enabled: obj.get("enabled").and_then(json_bool).unwrap_or(false),
        x: obj.get("x").and_then(json_f32).unwrap_or(default_x),
        y: obj.get("y").and_then(json_f32).unwrap_or(0.0),
    }
}

fn eval_bezier_segment(k0: &Keyframe, k1: &Keyframe, frame: f32) -> f32 {
    let df = (k1.frame - k0.frame).max(1e-5);
    let t = ((frame - k0.frame) / df).clamp(0.0, 1.0);
    let dv = k1.value - k0.value;

    if !k0.front.enabled && !k1.back.enabled {
        return k0.value + dv * t;
    }

    let x1 = if k0.front.enabled {
        (k0.front.x / 3.0).clamp(0.0, 1.0)
    } else {
        1.0 / 3.0
    };
    let y1 = if k0.front.enabled {
        k0.value + k0.front.y
    } else {
        k0.value + dv / 3.0
    };
    let x2 = if k1.back.enabled {
        (1.0 + k1.back.x / 3.0).clamp(0.0, 1.0)
    } else {
        2.0 / 3.0
    };
    let y2 = if k1.back.enabled {
        k1.value + k1.back.y
    } else {
        k1.value - dv / 3.0
    };

    // Solve B_x(u) = t on [0, 1] using safeguarded Newton-Raphson with bisection.
    let c = 3.0 * x1;
    let b = 3.0 * (x2 - x1) - c;
    let a = 1.0 - c - b;

    let mut lo = 0.0f32;
    let mut hi = 1.0f32;
    let mut u = t;
    for _ in 0..8 {
        let bx = ((a * u + b) * u + c) * u;
        let err = bx - t;
        if err.abs() < 1e-6 {
            break;
        }
        if err < 0.0 {
            lo = u;
        } else {
            hi = u;
        }
        let dbx = (3.0 * a * u + 2.0 * b) * u + c;
        let next = if dbx.abs() > 1e-6 {
            u - err / dbx
        } else {
            0.5 * (lo + hi)
        };
        u = if next > lo && next < hi {
            next
        } else {
            0.5 * (lo + hi)
        };
    }

    let omt = 1.0 - u;
    omt * omt * omt * k0.value
        + 3.0 * omt * omt * u * y1
        + 3.0 * omt * u * u * y2
        + u * u * u * k1.value
}

fn eval_curve(pts: &[Keyframe], frame: f32) -> f32 {
    let Some(first) = pts.first() else {
        return 0.0;
    };
    if frame <= first.frame {
        return first.value;
    }
    for w in pts.windows(2) {
        if frame <= w[1].frame {
            return eval_bezier_segment(&w[0], &w[1], frame);
        }
    }
    pts.last().map(|k| k.value).unwrap_or(0.0)
}

impl KeyTrack {
    /// Parse a property/constant object containing an `"animation"` definition.
    pub fn parse(v: &serde_json::Value) -> Option<Self> {
        let anim = v.get("animation")?.as_object()?;
        let opts = anim.get("options").and_then(|o| o.as_object());
        let name = opts
            .and_then(|o| o.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let fps = opts
            .and_then(|o| o.get("fps"))
            .and_then(json_f32)
            .unwrap_or(30.0)
            .max(1.0);
        let length = opts
            .and_then(|o| o.get("length"))
            .and_then(json_f32)
            .unwrap_or(60.0)
            .max(1.0);
        let mode = match opts.and_then(|o| o.get("mode")).and_then(|m| m.as_str()) {
            Some("mirror") => TrackMode::Mirror,
            Some("single") => TrackMode::Single,
            _ => TrackMode::Loop,
        };
        let start_paused = opts
            .and_then(|o| o.get("startpaused"))
            .and_then(json_bool)
            .unwrap_or(false);
        let wrap_loop = opts
            .and_then(|o| o.get("wraploop"))
            .and_then(json_bool)
            .unwrap_or(false);
        let relative = anim.get("relative").and_then(json_bool).unwrap_or(false);

        let mut curves = Vec::new();
        for key in ["c0", "c1", "c2", "c3"] {
            let mut pts: Vec<Keyframe> = anim
                .get(key)
                .and_then(|c| c.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|k| {
                            Some(Keyframe {
                                frame: k.get("frame").and_then(json_f32)?,
                                value: k.get("value").and_then(json_f32)?,
                                back: parse_handle(k.get("back"), -1.0),
                                front: parse_handle(k.get("front"), 1.0),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            if pts.is_empty() {
                break;
            }
            pts.sort_by(|a, b| {
                a.frame
                    .partial_cmp(&b.frame)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            curves.push(pts);
        }
        if curves.is_empty() {
            return None;
        }
        Some(Self {
            name,
            fps,
            length,
            mode,
            relative,
            start_paused,
            wrap_loop,
            curves,
            playing: !start_paused,
            rate: 1.0,
            frame: 0.0,
            mirror_reverse: false,
        })
    }

    pub fn mode_str(&self) -> &'static str {
        self.mode.as_str()
    }

    pub fn duration(&self) -> f32 {
        self.length / self.fps.max(1.0)
    }

    /// Advance the playhead by `dt` seconds. Returns `true` when a `single`
    /// animation reaches its end on this step and stops.
    pub fn advance(&mut self, dt: f32) -> bool {
        if !self.playing || dt <= 0.0 || self.rate == 0.0 {
            return false;
        }
        let delta = dt * self.fps * self.rate;
        match self.mode {
            TrackMode::Loop => {
                self.frame = (self.frame + delta).rem_euclid(self.length);
                false
            }
            TrackMode::Single => {
                let next = self.frame + delta;
                if delta >= 0.0 && next >= self.length {
                    self.frame = self.length;
                    self.playing = false;
                    true
                } else if delta < 0.0 && next <= 0.0 {
                    self.frame = 0.0;
                    self.playing = false;
                    true
                } else {
                    self.frame = next;
                    false
                }
            }
            TrackMode::Mirror => {
                let period = (self.length * 2.0).max(1e-4);
                let phase = if self.mirror_reverse {
                    period - self.frame
                } else {
                    self.frame
                };
                let m = (phase + delta).rem_euclid(period);
                if m <= self.length {
                    self.frame = m;
                    self.mirror_reverse = false;
                } else {
                    self.frame = period - m;
                    self.mirror_reverse = true;
                }
                false
            }
        }
    }

    /// Resume playback, restarting finished `Single` clips from the start.
    pub fn play(&mut self) {
        if self.mode == TrackMode::Single {
            if self.rate >= 0.0 && self.frame >= self.length - 1e-4 {
                self.frame = 0.0;
            } else if self.rate < 0.0 && self.frame <= 1e-4 {
                self.frame = self.length;
            }
        }
        self.playing = true;
    }

    pub fn pause(&mut self) {
        self.playing = false;
    }

    pub fn stop(&mut self) {
        self.playing = false;
        self.frame = 0.0;
        self.mirror_reverse = false;
    }

    pub fn set_frame(&mut self, frame: f32) {
        self.frame = frame.clamp(0.0, self.length);
        self.mirror_reverse = false;
    }

    /// Synchronize playback state sent by SceneScript's animation proxy.
    pub fn apply_script_state(&mut self, playing: bool, frame: f32, rate: f32, mirror_reverse: bool) {
        self.playing = playing;
        self.frame = frame.clamp(0.0, self.length);
        if rate.is_finite() {
            self.rate = rate;
        }
        self.mirror_reverse = mirror_reverse;
    }

    /// Preserve live playhead state when re-parsing authored properties.
    pub fn preserve_playback_from(&mut self, prev: &KeyTrack) {
        self.playing = prev.playing;
        self.rate = prev.rate;
        self.frame = prev.frame.clamp(0.0, self.length);
        self.mirror_reverse = prev.mirror_reverse;
    }

    /// Evaluate all curves at the current `frame`, broadcasting single-curve
    /// values across all four channels.
    pub fn eval(&self) -> [f32; 4] {
        let mut out = [0.0f32; 4];
        for (i, pts) in self.curves.iter().enumerate().take(4) {
            out[i] = eval_curve(pts, self.frame);
        }
        if self.curves.len() == 1 {
            out[1] = out[0];
            out[2] = out[0];
            out[3] = out[0];
        }
        out
    }

    pub fn eval3(&self) -> [f32; 3] {
        let v = self.eval();
        [v[0], v[1], v[2]]
    }

    pub fn apply3(&self, base: [f32; 3]) -> [f32; 3] {
        let v = self.eval3();
        if self.relative {
            [base[0] + v[0], base[1] + v[1], base[2] + v[2]]
        } else {
            v
        }
    }

    /// Evaluate as JSON scalar or vector matching the curve arity (`1..=4`).
    pub fn eval_value(&self) -> serde_json::Value {
        let v = self.eval();
        match self.curves.len() {
            0 | 1 => serde_json::json!(v[0]),
            2 => serde_json::json!([v[0], v[1]]),
            3 => serde_json::json!([v[0], v[1], v[2]]),
            _ => serde_json::json!([v[0], v[1], v[2], v[3]]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_handles_evaluate_linearly() {
        let raw = json!({
            "animation": {
                "options": { "fps": 10, "length": 10, "mode": "single" },
                "c0": [
                    { "frame": 0, "value": 0.0, "front": { "enabled": false, "x": 1, "y": 0 } },
                    { "frame": 10, "value": 100.0, "back": { "enabled": false, "x": -1, "y": 0 } }
                ]
            }
        });
        let mut tr = KeyTrack::parse(&raw).expect("track");
        tr.set_frame(2.5);
        assert!((tr.eval()[0] - 25.0).abs() < 1e-4);
        tr.set_frame(5.0);
        assert!((tr.eval()[0] - 50.0).abs() < 1e-4);
        tr.set_frame(7.5);
        assert!((tr.eval()[0] - 75.0).abs() < 1e-4);
    }

    #[test]
    fn default_enabled_handles_match_exact_smoothstep() {
        let raw = json!({
            "animation": {
                "options": { "fps": 10, "length": 10, "mode": "single" },
                "c0": [
                    { "frame": 0, "value": 10.0, "front": { "enabled": true, "x": 1.0, "y": 0.0 } },
                    { "frame": 10, "value": 20.0, "back": { "enabled": true, "x": -1.0, "y": 0.0 } }
                ]
            }
        });
        let mut tr = KeyTrack::parse(&raw).expect("track");
        for f in [0.0f32, 2.0, 4.0, 5.0, 8.0, 10.0] {
            tr.set_frame(f);
            let t = f / 10.0;
            let expected = 10.0 + 10.0 * (t * t * (3.0 - 2.0 * t));
            assert!(
                (tr.eval()[0] - expected).abs() < 1e-4,
                "frame {f}: got {}, expected {expected}",
                tr.eval()[0]
            );
        }
    }

    #[test]
    fn custom_asymmetric_bezier_handles_shape_curve() {
        let raw = json!({
            "animation": {
                "options": { "fps": 10, "length": 10, "mode": "single" },
                "c0": [
                    { "frame": 0, "value": 0.0, "front": { "enabled": true, "x": 0.2, "y": 80.0 } },
                    { "frame": 10, "value": 100.0, "back": { "enabled": true, "x": -2.5, "y": 0.0 } }
                ]
            }
        });
        let mut tr = KeyTrack::parse(&raw).expect("track");
        tr.set_frame(2.0);
        let early = tr.eval()[0];
        // Fast initial slope from y=+80 front handle must sit well above linear (20.0).
        assert!(early > 55.0, "expected steep rise, got {early}");
        tr.set_frame(8.0);
        let late = tr.eval()[0];
        assert!(late > early && late <= 100.0);
    }

    #[test]
    fn startpaused_and_single_replay_and_mirror_pingpong() {
        let raw = json!({
            "animation": {
                "options": { "fps": 10, "length": 10, "mode": "single", "startpaused": true },
                "c0": [
                    { "frame": 0, "value": 0.0 },
                    { "frame": 10, "value": 1.0 }
                ]
            }
        });
        let mut tr = KeyTrack::parse(&raw).expect("track");
        assert!(tr.start_paused);
        assert!(!tr.playing);
        assert!(!tr.advance(0.5));
        assert!((tr.frame - 0.0).abs() < 1e-6);

        tr.play();
        assert!(tr.playing);
        assert!(!tr.advance(0.5));
        assert!((tr.frame - 5.0).abs() < 1e-4);
        // Reaching end stops and returns true.
        assert!(tr.advance(0.6));
        assert!(!tr.playing);
        assert!((tr.frame - 10.0).abs() < 1e-4);

        // Second play() restarts single animation from frame 0.
        tr.play();
        assert!(tr.playing);
        assert!((tr.frame - 0.0).abs() < 1e-4);
        assert!(!tr.advance(0.3));
        assert!((tr.frame - 3.0).abs() < 1e-4);
        tr.stop();
        assert!(!tr.playing);
        assert!((tr.frame - 0.0).abs() < 1e-6);

        // Mirror ping-pong mode.
        let mraw = json!({
            "animation": {
                "options": { "fps": 10, "length": 10, "mode": "mirror" },
                "c0": [{ "frame": 0, "value": 0.0 }, { "frame": 10, "value": 1.0 }]
            }
        });
        let mut mt = KeyTrack::parse(&mraw).expect("mirror");
        mt.advance(1.2); // 12 frames -> bounces back to 8.0
        assert!((mt.frame - 8.0).abs() < 1e-4);
        assert!(mt.mirror_reverse);
        mt.advance(0.5); // 5 frames back -> 3.0
        assert!((mt.frame - 3.0).abs() < 1e-4);
        mt.set_frame(4.0);
        assert!(!mt.mirror_reverse);
        assert!((mt.frame - 4.0).abs() < 1e-4);
    }
}
