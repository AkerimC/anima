//! Wallpaper Engine SceneScript support.
//!
//! Wallpaper Engine scenes can bind almost any property to a small JavaScript
//! module (`"script": "...", "scriptproperties": {...}`).  The scripts are ES
//! modules with WE-specific globals: `engine`, `shared`, `thisScene`,
//! `thisLayer`, `createScriptProperties()` and the `Vec2`/`Vec3`/`Vec4`
//! helpers.  This module embeds QuickJS and provides those globals, then runs
//! one `update(value)` call per bound property and frame.
//!
//! The host is deliberately allocation-stable: one runtime/context per scene,
//! scripts compiled once at load, and a single JSON round trip per frame for
//! the properties that actually have scripts.

use std::collections::HashMap;

use rquickjs::{Context, Function, Runtime};
use serde_json::{json, Value};

/// Property fields that Wallpaper Engine allows scripts to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScriptField {
    Visible,
    Alpha,
    Origin,
    Scale,
    Angles,
    Color,
    Text,
}

impl ScriptField {
    pub fn as_str(self) -> &'static str {
        match self {
            ScriptField::Visible => "visible",
            ScriptField::Alpha => "alpha",
            ScriptField::Origin => "origin",
            ScriptField::Scale => "scale",
            ScriptField::Angles => "angles",
            ScriptField::Color => "color",
            ScriptField::Text => "text",
        }
    }
}

/// A layer as the scripts see it (`thisLayer` / `thisScene.getLayer()`).
#[derive(Debug, Clone)]
pub struct LayerSeed {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub visible: bool,
    pub alpha: f32,
    pub origin: [f32; 3],
    pub scale: [f32; 3],
    pub angles: [f32; 3],
    pub color: [f32; 3],
    pub size: [f32; 2],
    pub align: [f32; 2],
    pub text: String,
    pub parent: Option<i64>,
}

/// Per-frame state of a layer that has script bindings.
#[derive(Debug, Clone)]
pub struct LayerFrame {
    pub id: i64,
    pub visible: bool,
    pub alpha: f32,
    pub origin: [f32; 3],
    pub scale: [f32; 3],
    pub angles: [f32; 3],
    pub color: [f32; 3],
    pub text: String,
}

/// Result of one scripted property for this frame.
#[derive(Debug, Clone)]
pub struct ScriptOutput {
    pub id: i64,
    pub field: ScriptField,
    pub value: Value,
}

pub struct SceneScripts {
    #[allow(dead_code)]
    rt: Runtime,
    ctx: Context,
    /// One entry per compiled property binding.
    bindings: Vec<Binding>,
    /// Script compile/runtime failures, logged once per binding.
    reported: HashMap<usize, u8>,
    /// First few distinct failure messages, for diagnostics and tests.
    errors: Vec<String>,
    timeouts: usize,
}

struct Binding {
    id: i64,
    field: ScriptField,
    enabled: bool,
}

impl SceneScripts {
    /// Create the QuickJS host.  Returns `None` when the runtime cannot start
    /// (never fatal for rendering — the scene simply keeps authored values).
    pub fn new(user_properties: &serde_json::Map<String, Value>) -> Option<Self> {
        let rt = Runtime::new().ok()?;
        let ctx = Context::full(&rt).ok()?;
        if let Err(err) = ctx.with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(PRELUDE.as_bytes())
        }) {
            log::warn!("scenescript: prelude failed: {err}");
            return None;
        }
        let seed = json!({ "userProperties": user_properties });
        if let Err(err) = ctx.with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            let f: Function = globals.get("__WE_init")?;
            f.call::<_, ()>((seed.to_string(),))
        }) {
            log::warn!("scenescript: user properties failed: {err}");
        }
        Some(Self {
            rt,
            ctx,
            bindings: Vec::new(),
            reported: HashMap::new(),
            errors: Vec::new(),
            timeouts: 0,
        })
    }

    pub fn add_layer(&mut self, seed: &LayerSeed) {
        let payload = json!({
            "id": seed.id,
            "name": seed.name,
            "kind": seed.kind,
            "visible": seed.visible,
            "alpha": seed.alpha,
            "origin": seed.origin,
            "scale": seed.scale,
            "angles": seed.angles,
            "color": seed.color,
            "size": seed.size,
            "align": seed.align,
            "text": seed.text,
            "parent": seed.parent,
        });
        if let Err(err) = self.call_unit("__WE_addLayer", &payload.to_string()) {
            self.report_once(usize::MAX, "layer", &err);
        }
    }

    /// Compile and register one property script.  `script_props` is the
    /// field's own `scriptproperties` block (with `{user: ...}` entries
    /// already resolved), which overrides the script's declared defaults.
    pub fn add_property_script(
        &mut self,
        id: i64,
        field: ScriptField,
        script: &str,
        initial: Value,
        script_props: &serde_json::Map<String, Value>,
    ) {
        let payload = json!({
            "id": id,
            "field": field.as_str(),
            "source": script,
            "initial": initial,
            "scriptProperties": script_props,
        });
        match self.call_unit("__WE_addScript", &payload.to_string()) {
            Ok(()) => self.bindings.push(Binding {
                id,
                field,
                enabled: true,
            }),
            Err(err) => {
                let idx = self.bindings.len();
                self.bindings.push(Binding {
                    id,
                    field,
                    enabled: false,
                });
                self.report_once(idx, "compile", &err);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Resolve `parent` ids into `thisLayer.getParent()` links.  Called once
    /// after every layer of the scene has been registered.
    pub fn link_parents(&mut self) {
        if let Err(err) = self.call_unit("__WE_linkParents", "") {
            self.report_once(usize::MAX, "layer", &err);
        }
    }

    /// Number of bindings disabled by a compile or runtime failure.
    pub fn failures(&self) -> usize {
        self.bindings.iter().filter(|b| !b.enabled).count()
    }

    /// First few distinct failure messages (one line each).
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Bound `(object id, field)` pairs, so the renderer knows which values it
    /// has to feed back into the tick.
    pub fn bound_fields(&self) -> impl Iterator<Item = (i64, ScriptField)> + '_ {
        self.bindings.iter().map(|b| (b.id, b.field))
    }

    /// Run one frame of scripts.  `layers` carries the current authored /
    /// animated state of every layer that has at least one binding.
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        runtime: f32,
        frametime: f32,
        time_of_day: f32,
        mouse: [f32; 2],
        canvas: [f32; 2],
        screen: [f32; 2],
        layers: &[LayerFrame],
    ) -> Vec<ScriptOutput> {
        if self.bindings.is_empty() {
            return Vec::new();
        }
        let mut layer_json = Vec::with_capacity(layers.len());
        for l in layers {
            layer_json.push(json!({
                "id": l.id,
                "visible": l.visible,
                "alpha": l.alpha,
                "origin": l.origin,
                "scale": l.scale,
                "angles": l.angles,
                "color": l.color,
                "text": l.text,
            }));
        }
        let payload = json!({
            "runtime": runtime,
            "frametime": frametime,
            "timeOfDay": time_of_day,
            "mouse": mouse,
            "canvas": canvas,
            "screen": screen,
            "layers": layer_json,
        });

        let raw = match self.call_string("__WE_tick", &payload.to_string()) {
            Ok(raw) => raw,
            Err(err) => {
                self.report_once(usize::MAX, "tick", &err);
                return Vec::new();
            }
        };
        let Ok(values) = serde_json::from_str::<Vec<Value>>(&raw) else {
            self.report_once(usize::MAX, "tick-output", "invalid JSON from script host");
            return Vec::new();
        };

        let mut out = Vec::new();
        for (idx, entry) in values.into_iter().enumerate() {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            if let Some(err) = obj.get("error").and_then(|v| v.as_str()) {
                let binding = self.bindings.get(idx);
                if let Some(b) = binding {
                    if !b.enabled {
                        continue;
                    }
                }
                self.report_once(idx, "runtime", err);
                if let Some(b) = self.bindings.get_mut(idx) {
                    b.enabled = false;
                }
                continue;
            }
            let Some(binding) = self.bindings.get(idx) else {
                continue;
            };
            if !binding.enabled {
                continue;
            }
            let Some(value) = obj.get("value") else {
                continue;
            };
            out.push(ScriptOutput {
                id: binding.id,
                field: binding.field,
                value: value.clone(),
            });
        }
        self.timeouts += 1;
        out
    }

    fn report_once(&mut self, idx: usize, kind: &str, err: &str) {
        let count = self.reported.entry(idx).or_insert(0);
        if *count >= 2 {
            return;
        }
        *count += 1;
        if self.errors.len() < 64 {
            self.errors.push(err.lines().next().unwrap_or(err).to_string());
        }
        if idx == usize::MAX {
            log::warn!("scenescript: {kind}: {err}");
        } else if let Some(b) = self.bindings.get(idx) {
            log::warn!(
                "scenescript: {} on object {}: {err}",
                b.field.as_str(),
                b.id
            );
        } else {
            log::warn!("scenescript: {kind}: {err}");
        }
    }

    fn call_unit(&mut self, name: &str, payload: &str) -> Result<(), String> {
        let name = name.to_string();
        let payload = payload.to_string();
        self.ctx
            .with(|ctx| -> rquickjs::Result<()> {
                let globals = ctx.globals();
                let f: Function = globals.get(name.as_str())?;
                f.call::<_, ()>((payload,))
            })
            .map_err(|e| e.to_string())
    }

    fn call_string(&mut self, name: &str, payload: &str) -> Result<String, String> {
        let name = name.to_string();
        let payload = payload.to_string();
        self.ctx
            .with(|ctx| -> rquickjs::Result<String> {
                let globals = ctx.globals();
                let f: Function = globals.get(name.as_str())?;
                f.call::<_, String>((payload,))
            })
            .map_err(|e| e.to_string())
    }
}

/// QuickJS prelude: WE globals, math helpers, layer proxies and the script
/// loader.  Everything is plain JS so the module preprocessor below only has
/// to deal with imports/exports.
const PRELUDE: &str = r#"
(function () {
  'use strict';

  function num(v, d) { return typeof v === 'number' && isFinite(v) ? v : (d || 0); }

  function normalizeField(field, v) {
    if (field === 'origin' || field === 'scale' || field === 'angles' || field === 'color') {
      return parseVec(v);
    }
    if (field === 'visible') return v === undefined || v === null ? true : !!v;
    if (field === 'alpha') return num(v, 1);
    if (field === 'text') return v === undefined || v === null ? '' : String(v);
    return v;
  }

  function errText(e) {
    if (e === undefined || e === null) return 'unknown error';
    var msg = (e.name ? e.name + ': ' : '') + (e.message !== undefined ? e.message : String(e));
    var st = e.stack ? String(e.stack).split('\n') : [];
    for (var i = 0; i < st.length; i++) {
      if (/^\s+at\s/.test(st[i])) { msg += ' @' + st[i].trim(); break; }
    }
    return msg;
  }

  var ZERO3 = { x: 0, y: 0, z: 0 };
  function safe3(v) { return v && typeof v === 'object' ? v : ZERO3; }
  var ZERO2 = { x: 0, y: 0 };
  function safe2(v) { return v && typeof v === 'object' ? v : ZERO2; }

  function Vec2(x, y) {
    if (x && typeof x === 'object') {
      this.x = num(x.x, 0);
      this.y = num(x.y !== undefined ? x.y : y, 0);
      return;
    }
    if (y === undefined && typeof x === 'number') { this.x = x; this.y = x; return; }
    this.x = num(x, 0); this.y = num(y, 0);
  }
  Vec2.prototype.add = function (o) { o = safe2(o); return new Vec2(this.x + num(o.x, 0), this.y + num(o.y, 0)); };
  Vec2.prototype.subtract = function (o) { o = safe2(o); return new Vec2(this.x - num(o.x, 0), this.y - num(o.y, 0)); };
  Vec2.prototype.multiply = function (o) {
    if (o && typeof o === 'object') return new Vec2(this.x * num(o.x, 1), this.y * num(o.y, 1));
    return new Vec2(this.x * num(o, 1), this.y * num(o, 1));
  };
  Vec2.prototype.divide = function (o) {
    if (o && typeof o === 'object') return new Vec2(this.x / (num(o.x, 1) || 1), this.y / (num(o.y, 1) || 1));
    return new Vec2(this.x / (num(o, 1) || 1), this.y / (num(o, 1) || 1));
  };
  Vec2.prototype.mix = function (o, t) {
    o = safe2(o); t = num(t, 0);
    return new Vec2(this.x + (num(o.x, 0) - this.x) * t, this.y + (num(o.y, 0) - this.y) * t);
  };
  Vec2.prototype.length = function () { return Math.hypot(this.x, this.y); };
  Vec2.prototype.normalize = function () { var l = this.length() || 1; return new Vec2(this.x / l, this.y / l); };
  Vec2.prototype.copy = function () { return new Vec2(this.x, this.y); };
  Vec2.prototype.clone = Vec2.prototype.copy;
  Vec2.prototype.toVec3 = function () { return new Vec3(this.x, this.y, 0); };
  Vec2.prototype.equals = function (o) { return !!o && this.x === o.x && this.y === o.y; };
  Vec2.prototype.toString = function () { return this.x + ' ' + this.y; };

  function Vec3(x, y, z) {
    if (x && typeof x === 'object') {
      this.x = num(x.x, 0);
      this.y = num(x.y, 0);
      this.z = num(x.z !== undefined ? x.z : z, 0);
      return;
    }
    if (y === undefined && z === undefined && typeof x === 'number') {
      this.x = x; this.y = x; this.z = x;
      return;
    }
    this.x = num(x, 0); this.y = num(y, 0); this.z = num(z, 0);
  }
  Vec3.prototype.add = function (o) { o = safe3(o); return new Vec3(this.x + num(o.x, 0), this.y + num(o.y, 0), this.z + num(o.z, 0)); };
  Vec3.prototype.subtract = function (o) { o = safe3(o); return new Vec3(this.x - num(o.x, 0), this.y - num(o.y, 0), this.z - num(o.z, 0)); };
  Vec3.prototype.multiply = function (o) {
    if (o && typeof o === 'object') return new Vec3(this.x * num(o.x, 1), this.y * num(o.y, 1), this.z * num(o.z, 1));
    return new Vec3(this.x * num(o, 1), this.y * num(o, 1), this.z * num(o, 1));
  };
  Vec3.prototype.divide = function (o) {
    if (o && typeof o === 'object') return new Vec3(this.x / (num(o.x, 1) || 1), this.y / (num(o.y, 1) || 1), this.z / (num(o.z, 1) || 1));
    var d = num(o, 1) || 1;
    return new Vec3(this.x / d, this.y / d, this.z / d);
  };
  Vec3.prototype.mix = function (o, t) {
    o = safe3(o); t = num(t, 0);
    return new Vec3(
      this.x + (num(o.x, 0) - this.x) * t,
      this.y + (num(o.y, 0) - this.y) * t,
      this.z + (num(o.z, 0) - this.z) * t
    );
  };
  Vec3.prototype.dot = function (o) { o = safe3(o); return this.x * o.x + this.y * o.y + this.z * o.z; };
  Vec3.prototype.cross = function (o) {
    o = safe3(o);
    return new Vec3(this.y * o.z - this.z * o.y, this.z * o.x - this.x * o.z, this.x * o.y - this.y * o.x);
  };
  Vec3.prototype.length = function () { return Math.sqrt(this.lengthSqr()); };
  Vec3.prototype.lengthSqr = function () { return this.x * this.x + this.y * this.y + this.z * this.z; };
  Vec3.prototype.normalize = function () { var l = this.length() || 1; return new Vec3(this.x / l, this.y / l, this.z / l); };
  Vec3.prototype.copy = function () { return new Vec3(this.x, this.y, this.z); };
  Vec3.prototype.clone = Vec3.prototype.copy;
  Vec3.prototype.toVec2 = function () { return new Vec2(this.x, this.y); };
  Vec3.prototype.equals = function (o) { return !!o && this.x === o.x && this.y === o.y && this.z === o.z; };
  Vec3.prototype.toString = function () { return this.x + ' ' + this.y + ' ' + this.z; };

  function Vec4(x, y, z, w) {
    if (x && typeof x === 'object') {
      this.x = num(x.x, 0); this.y = num(x.y, 0);
      this.z = num(x.z, 0); this.w = num(x.w, 0);
      return;
    }
    this.x = num(x, 0); this.y = num(y, 0); this.z = num(z, 0); this.w = num(w, 0);
  }
  Vec4.prototype.copy = function () { return new Vec4(this.x, this.y, this.z, this.w); };
  Vec4.prototype.clone = Vec4.prototype.copy;
  Vec4.prototype.multiply = function (o) {
    if (o && typeof o === 'object') return new Vec4(this.x * num(o.x, 1), this.y * num(o.y, 1), this.z * num(o.z, 1), this.w * num(o.w, 1));
    var d = num(o, 1);
    return new Vec4(this.x * d, this.y * d, this.z * d, this.w * d);
  };
  Vec4.prototype.toString = function () { return this.x + ' ' + this.y + ' ' + this.z + ' ' + this.w; };

  globalThis.Vec2 = Vec2;
  globalThis.Vec3 = Vec3;
  globalThis.Vec4 = Vec4;

  var WEMath = {
    mix: function (a, b, t) { return a + (b - a) * t; },
    lerp: function (a, b, t) { return a + (b - a) * t; },
    smoothStep: function (e0, e1, x) {
      var t = Math.min(Math.max((x - e0) / ((e1 - e0) || 1), 0), 1);
      return t * t * (3 - 2 * t);
    },
    clamp: function (x, a, b) { return Math.min(Math.max(x, a), b); },
    saturate: function (x) { return Math.min(Math.max(x, 0), 1); },
    deg2rad: function (d) { return d * Math.PI / 180; },
    rad2deg: function (r) { return r * 180 / Math.PI; },
    random: function () { return Math.random(); },
    sign: function (x) { return x < 0 ? -1 : (x > 0 ? 1 : 0); },
    step: function (e, x) { return x < e ? 0 : 1; },
    abs: Math.abs
  };
  var WEColor = {
    hsv2rgb: function (h, s, v) {
      h = ((h % 1) + 1) % 1;
      var i = Math.floor(h * 6), f = h * 6 - i;
      var p = v * (1 - s), q = v * (1 - f * s), t = v * (1 - (1 - f) * s);
      var r, g, b;
      switch (i % 6) {
        case 0: r = v; g = t; b = p; break;
        case 1: r = q; g = v; b = p; break;
        case 2: r = p; g = v; b = t; break;
        case 3: r = p; g = q; b = v; break;
        case 4: r = t; g = p; b = v; break;
        default: r = v; g = p; b = q; break;
      }
      return new Vec3(r, g, b);
    },
    rgb2hsv: function (c) {
      var max = Math.max(c.x, c.y, c.z), min = Math.min(c.x, c.y, c.z);
      var d = max - min, h = 0;
      if (d > 0) {
        if (max === c.x) h = ((c.y - c.z) / d) % 6;
        else if (max === c.y) h = (c.z - c.x) / d + 2;
        else h = (c.x - c.y) / d + 4;
        h /= 6;
      }
      return new Vec3(((h % 1) + 1) % 1, max > 0 ? d / max : 0, max);
    }
  };
  var WEVector = {
    angleVector2: function (a) { return new Vec2(Math.cos(a), Math.sin(a)); }
  };
  globalThis.__WE_MODULES = { WEMath: WEMath, WEColor: WEColor, WEVector: WEVector };

  var userProperties = {};
  var timers = [];
  var nextTimer = 1;

  globalThis.engine = {
    runtime: 0,
    frametime: 0,
    timeOfDay: 0,
    canvasSize: new Vec2(1920, 1080),
    screenResolution: new Vec2(1920, 1080),
    mousePosition: new Vec2(0, 0),
    userProperties: userProperties,
    isScreensaver: false,
    AUDIO_RESOLUTION_16: 16,
    AUDIO_RESOLUTION_32: 32,
    AUDIO_RESOLUTION_64: 64,
    registerAudioBuffers: function (res) { return audioBuffers(res); },
    isScreensaver: function () { return false; },
    registerAsset: function (p) { return p; },
    openUserShortcut: function () { return 0; },
    createAnimation: function () { return animStub(); },
    createEffect: function () { return {}; },
    setTimeout: function (fn, ms) {
      var id = nextTimer++;
      timers.push({ id: id, fn: fn, at: globalThis.engine.runtime + (ms || 0) / 1000 });
      return id;
    },
    clearTimeout: function (id) { timers = timers.filter(function (t) { return t.id !== id; }); }
  };

  globalThis.shared = {};

  var audioCache = {};
  function audioBuffers(res) {
    var n = Math.max(1, Math.min(256, num(res, 16)));
    var buf = audioCache[n];
    if (!buf) {
      function zeros() { var a = []; for (var i = 0; i < n; i++) a.push(0); return a; }
      buf = audioCache[n] = { length: n, left: zeros(), right: zeros(), average: zeros() };
    }
    return buf;
  }

  // Wallpaper Engine persists small values with get/set/remove.
  var storage = {};
  globalThis.localStorage = {
    get: function (k, d) { var v = storage[k]; return v === undefined ? (d === undefined ? null : d) : v; },
    set: function (k, v) { storage[k] = v; },
    remove: function (k) { delete storage[k]; },
    clear: function () { storage = {}; }
  };

  globalThis.console = {
    log: function () {}, info: function () {}, warn: function () {},
    error: function () {}, debug: function () {}, trace: function () {}
  };

  globalThis.MediaPlaybackEvent = {
    PLAYBACK_STOPPED: 0, PLAYBACK_PLAYING: 1, PLAYBACK_PAUSED: 2
  };

  globalThis.wallpaperPluginProps = globalThis.wallpaperPluginProps || {};

  globalThis.input = {
    cursorWorldPosition: new Vec3(0, 0, 0),
    cursorScreenPosition: new Vec2(0, 0),
    cursorLeftDown: false,
    cursorRightDown: false,
    cursorSize: new Vec2(32, 32)
  };

  globalThis.thisObject = null;

  function parseVec(v) {
    if (Array.isArray(v)) return new Vec3(v[0], v[1], v[2]);
    if (v && typeof v === 'object') return new Vec3(v.x, v.y, v.z);
    if (typeof v === 'string') {
      var p = v.trim().split(/\s+/).map(parseFloat);
      return new Vec3(p[0], p[1], p[2]);
    }
    return new Vec3(0, 0, 0);
  }
  function toArr3(v) {
    if (Array.isArray(v)) return [num(v[0], 0), num(v[1], 0), num(v[2], 0)];
    if (v && typeof v === 'object') return [num(v.x, 0), num(v.y, 0), num(v.z, 0)];
    var n = typeof v === 'number' ? v : 0;
    return [n, n, n];
  }

  var layers = {};
  var layerOrder = [];
  var scripts = [];

  function layerByRef(ref) {
    if (ref == null) return undefined;
    if (typeof ref === 'object') return ref;
    if (typeof ref === 'number') return layers[String(ref)];
    if (layers[String(ref)]) return layers[String(ref)];
    for (var i = 0; i < layerOrder.length; i++) {
      if (layers[layerOrder[i]].name === ref) return layers[layerOrder[i]];
    }
    return undefined;
  }

  function animStub() {
    var a = {
      rate: 1, currentFrame: 0, frameCount: 0, duration: 0, __playing: false,
      play: function () { a.__playing = true; },
      pause: function () { a.__playing = false; },
      stop: function () { a.__playing = false; a.currentFrame = 0; },
      isPlaying: function () { return a.__playing; },
      setFrame: function (f) { a.currentFrame = num(f, 0); },
      getFrame: function () { return a.currentFrame; },
      setPlaybackRate: function (r) { a.rate = num(r, 1); },
      getPlaybackRate: function () { return a.rate; }
    };
    return a;
  }

  function videoStub() {
    var v = {
      currentTime: 0, duration: 0, playbackRate: 1, __playing: false, muted: true, volume: 0,
      play: function () { v.__playing = true; },
      pause: function () { v.__playing = false; },
      stop: function () { v.__playing = false; v.currentTime = 0; },
      isPlaying: function () { return v.__playing; },
      setCurrentTime: function (t) { v.currentTime = num(t, 0); },
      getCurrentTime: function () { return v.currentTime; },
      setPlaybackRate: function (r) { v.playbackRate = num(r, 1); },
      getPlaybackRate: function () { return v.playbackRate; },
      setMuted: function (m) { v.muted = !!m; },
      isMuted: function () { return v.muted; }
    };
    return v;
  }

  function soundStub() {
    var s = { volume: 1, muted: true, __playing: false,
      play: function () { s.__playing = true; },
      pause: function () { s.__playing = false; },
      stop: function () { s.__playing = false; s.time = 0; },
      isPlaying: function () { return s.__playing; },
      setVolume: function (v) { s.volume = num(v, 1); },
      getVolume: function () { return s.volume; },
      setMuted: function (m) { s.muted = !!m; },
      isMuted: function () { return s.muted; },
      getCurrentTime: function () { return 0; },
      setCurrentTime: function () {} };
    return s;
  }

  function makeLayer(id, name) {
    var l = {
      id: id, name: name || '', visible: true, alpha: 1,
      origin: new Vec3(0, 0, 0), scale: new Vec3(1, 1, 1), angles: new Vec3(0, 0, 0),
      color: new Vec3(1, 1, 1), size: new Vec2(0, 0), alignment: new Vec2(0, 0),
      text: '', pointsize: 0, cursorDetected: false, clicked: false, kind: '',
      __parent: null, __tex: null, __video: null, __layerFile: null,
      getParent: function () { return l.__parent || null; },
      getTextureAnimation: function () {
        if (!l.__tex) { l.__tex = animStub(); l.__tex.frameCount = 1; }
        return l.__tex;
      },
      getVideoTexture: function () {
        if (!l.__video) l.__video = videoStub();
        return l.__video;
      },
      getAnimation: function () { return l.getTextureAnimation(); },
      getSound: function () { return soundStub(); },
      getLayerFile: function () { return l.__layerFile; },
      getTransformMatrix: function () {
        return { m: [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, l.origin.x, l.origin.y, 0, 1] };
      },
      isPlaying: function () { return false; },
      play: function () {}, pause: function () {}, stop: function () {}
    };
    return l;
  }

  var scene = {
    getLayer: function (ref) { return layerByRef(ref); },
    enumerateLayers: function () { return layerOrder.map(function (id) { return layers[id]; }); },
    getLayerIndex: function (l) { var i = layerOrder.indexOf(String(l && l.id)); return i < 0 ? -1 : i; },
    sortLayer: function () { return true; },
    destroyLayer: function () { return true; },
    createLayer: function (cfg) { return makeLayer(-1, cfg && cfg.name); },
    getInitialLayerConfig: function (ref) {
      var l = layerByRef(ref);
      if (!l) return {};
      return {
        name: l.name,
        size: new Vec2(l.size.x, l.size.y),
        scale: new Vec3(l.scale.x, l.scale.y, l.scale.z),
        origin: new Vec3(l.origin.x, l.origin.y, l.origin.z),
        angles: new Vec3(l.angles.x, l.angles.y, l.angles.z),
        color: new Vec3(l.color.x, l.color.y, l.color.z),
        alpha: l.alpha,
        visible: l.visible,
        text: l.text,
        image: l.image || '',
        alignment: l.alignment ? l.alignment.copy() : new Vec2(0, 0)
      };
    },
    getAnimation: animStub,
    getTextureAnimation: animStub
  };

  globalThis.thisScene = scene;
  globalThis.thisLayer = null;

  function userValue(name, fallback) {
    var v = userProperties[name];
    if (v === undefined || v === null) return fallback;
    return v;
  }

  globalThis.createScriptProperties = function () {
    var props = {};
    var override = globalThis.__WE_binding_props || {};
    function read(name, fallback) {
      var v = override[name];
      if (v === undefined) v = userProperties[name];
      return (v === undefined || v === null) ? fallback : v;
    }
    var api = {
      addSlider: function (o) { props[o.name] = num(read(o.name, o.value), 0); return api; },
      addCheckbox: function (o) { props[o.name] = !!read(o.name, o.value); return api; },
      addCombo: function (o) { props[o.name] = read(o.name, o.value); return api; },
      addText: function (o) { var v = read(o.name, o.value || ''); props[o.name] = String(v); return api; },
      addColor: function (o) { props[o.name] = parseVec(read(o.name, o.value)); return api; },
      addDirection: function (o) { props[o.name] = parseVec(read(o.name, o.value)); return api; },
      addFile: function (o) { var v = read(o.name, o.value || ''); props[o.name] = String(v); return api; },
      addFont: function (o) { var v = read(o.name, o.value || ''); props[o.name] = String(v); return api; },
      finish: function () { return props; }
    };
    return api;
  };

  function prepare(src) {
    var names = [];
    var body = src;
    body = body.replace(/import[ \t]*\*[ \t]*as[ \t]+([A-Za-z_$][\w$]*)[ \t]*from[ \t]*['"]([^'"]+)['"]/g,
      function (m, alias, mod) { return 'var ' + alias + ' = __WE_MODULES[' + JSON.stringify(mod) + ']'; });
    body = body.replace(/import[ \t]+([A-Za-z_$][\w$]*)[ \t]*from[ \t]*['"]([^'"]+)['"]/g,
      function (m, alias, mod) { return 'var ' + alias + ' = __WE_MODULES[' + JSON.stringify(mod) + ']'; });
    body = body.replace(/export[ \t]+(async[ \t]+)?function[ \t]+([A-Za-z_$][\w$]*)/g,
      function (m, a, n) { names.push(n); return (a || '') + 'function ' + n; });
    body = body.replace(/export[ \t]+(var|let|const)[ \t]+([A-Za-z_$][\w$]*)/g,
      function (m, k, n) { names.push(n); return k + ' ' + n; });
    body = body.replace(/export[ \t]+class[ \t]+([A-Za-z_$][\w$]*)/g,
      function (m, n) { names.push(n); return 'class ' + n; });
    body = body.replace(/export[ \t]*\{([^}]*)\}[ \t]*;?/g, function (m, list) {
      list.split(',').forEach(function (item) {
        var bits = item.trim().split(/\s+as\s+/);
        if (bits[0]) names.push(bits[1] || bits[0]);
      });
      return '';
    });
    body = body.replace(/export[ \t]+default[ \t]+/g, '');
    return { body: body, names: names };
  }

  var init = {
    addLayer: function (jsonText) {
      var d = JSON.parse(jsonText);
      var l = makeLayer(d.id, d.name);
      l.kind = d.kind || '';
      l.visible = d.visible !== false;
      l.alpha = num(d.alpha, 1);
      l.origin = parseVec(d.origin);
      l.scale = parseVec(d.scale);
      l.angles = parseVec(d.angles);
      l.color = parseVec(d.color);
      l.size = new Vec2(d.size && d.size[0], d.size && d.size[1]);
      l.alignment = new Vec2(d.align && d.align[0], d.align && d.align[1]);
      l.text = d.text || '';
      l.__parentId = d.parent;
      layers[String(d.id)] = l;
      layerOrder.push(String(d.id));
      return true;
    },
    linkParents: function () {
      for (var i = 0; i < layerOrder.length; i++) {
        var l = layers[layerOrder[i]];
        if (l.__parentId != null) l.__parent = layers[String(l.__parentId)] || null;
      }
      return true;
    },
    addScript: function (jsonText) {
      var d = JSON.parse(jsonText);
      d.initial = normalizeField(d.field, d.initial);
      var prepared = prepare(d.source);
      var factory = (0, eval)('(function(){\n' + prepared.body + '\nreturn {' +
        prepared.names.map(function (n) {
          return n + ':(typeof ' + n + '===\'undefined\'?undefined:' + n + ')';
        }).join(',') + '};\n})');
      var mod;
      globalThis.__WE_binding_props = d.scriptProperties || {};
      try {
        mod = factory();
      } finally {
        globalThis.__WE_binding_props = {};
      }
      var layer = layers[String(d.id)];
      var entry = {
        id: d.id, field: d.field, mod: mod, layer: layer || null,
        source: d.source, initial: d.initial, dead: false, initDone: false
      };
      if (mod.init) {
        try {
          globalThis.thisLayer = layer;
          globalThis.thisObject = layer;
          var returned = mod.init(entry.initial);
          if (returned !== undefined) entry.initial = returned;
        } catch (e) {
          entry.dead = true;
          entry.error = errText(e);
        }
        entry.initDone = true;
      }
      scripts.push(entry);
      return true;
    },
    tick: function (jsonText) {
      var s = JSON.parse(jsonText);
      var e = globalThis.engine;
      e.runtime = s.runtime || 0;
      e.frametime = s.frametime || 0;
      e.timeOfDay = s.timeOfDay || 0;
      e.mousePosition = new Vec2(s.mouse && s.mouse[0], s.mouse && s.mouse[1]);
      e.canvasSize = new Vec2(s.canvas && s.canvas[0], s.canvas && s.canvas[1]);
      e.screenResolution = new Vec2(s.screen && s.screen[0], s.screen && s.screen[1]);
      globalThis.input.cursorWorldPosition = new Vec3(e.mousePosition.x, e.mousePosition.y, 0);

      for (var i = 0; i < s.layers.length; i++) {
        var d = s.layers[i];
        var l = layers[String(d.id)];
        if (!l) continue;
        l.visible = d.visible !== false;
        l.alpha = num(d.alpha, 1);
        l.origin = parseVec(d.origin);
        l.scale = parseVec(d.scale);
        l.angles = parseVec(d.angles);
        l.color = parseVec(d.color);
        l.text = d.text || '';
      }

      while (timers.length) {
        var due = null;
        for (var t = 0; t < timers.length; t++) {
          if (timers[t].at <= e.runtime) { due = timers[t]; timers.splice(t, 1); break; }
        }
        if (!due) break;
        try { due.fn(); } catch (err) { /* keep other timers alive */ }
      }

      var out = [];
      for (var k = 0; k < scripts.length; k++) {
        var b = scripts[k];
        var layer = b.layer;
        if (!layer) { out.push({ error: 'layer missing' }); continue; }
        if (b.dead) { out.push({ error: b.error || 'dead' }); continue; }
        var value;
        if (b.field === 'text') value = layer.text;
        else if (b.field === 'visible') value = layer.visible;
        else if (b.field === 'alpha') value = layer.alpha;
        else value = layer[b.field] ? layer[b.field].copy() : null;

        try {
          globalThis.thisLayer = layer;
          globalThis.thisScene = scene;
          globalThis.thisObject = layer;
          var res = b.mod.update ? b.mod.update(value) : undefined;
          if (b.field === 'text') {
            if (res !== undefined && res !== null) layer.text = String(res);
            value = layer.text;
          } else if (b.field === 'visible') {
            if (res !== undefined) layer.visible = !!res;
            value = layer.visible;
          } else if (b.field === 'alpha') {
            if (res !== undefined) layer.alpha = num(res, layer.alpha);
            value = layer.alpha;
          } else {
            var v = (res !== undefined && res !== null) ? res : layer[b.field];
            value = toArr3(v);
            if (b.field === 'color') layer.color = parseVec(v);
            else layer[b.field] = parseVec(v);
          }
          out.push({ id: b.id, field: b.field, value: value });
        } catch (err) {
          out.push({ error: errText(err) });
        }
      }
      return JSON.stringify(out);
    }
  };

  globalThis.__WE_init = function (jsonText) {
    var d = JSON.parse(jsonText);
    userProperties = d.userProperties || {};
    globalThis.engine.userProperties = userProperties;
    return true;
  };
  globalThis.__WE_addLayer = init.addLayer;
  globalThis.__WE_addScript = init.addScript;
  globalThis.__WE_tick = init.tick;
  globalThis.__WE_linkParents = init.linkParents;
})();
"#;

/// Parse a Wallpaper Engine vector property (`"x y z"`, `[x,y,z]` or
/// `{"x":…}`) into a 3-component array.
pub fn prop_vec3(v: &Value, default: [f32; 3]) -> [f32; 3] {
    match v {
        Value::String(s) => {
            let mut out = default;
            for (i, part) in s.split_whitespace().take(3).enumerate() {
                if let Ok(n) = part.parse::<f32>() {
                    out[i] = n;
                }
            }
            out
        }
        Value::Array(a) => {
            let mut out = default;
            for (i, part) in a.iter().take(3).enumerate() {
                if let Some(n) = part.as_f64() {
                    out[i] = n as f32;
                }
            }
            out
        }
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0) as f32;
            [f, f, f]
        }
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(id: i64, name: &str) -> LayerSeed {
        LayerSeed {
            id,
            name: name.to_string(),
            kind: "image".into(),
            visible: true,
            alpha: 1.0,
            origin: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
            angles: [0.0, 0.0, 0.0],
            color: [1.0, 1.0, 1.0],
            size: [100.0, 100.0],
            align: [0.0, 0.0],
            text: String::new(),
            parent: None,
        }
    }

    fn frame(id: i64) -> LayerFrame {
        LayerFrame {
            id,
            visible: true,
            alpha: 1.0,
            origin: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
            angles: [0.0, 0.0, 0.0],
            color: [1.0, 1.0, 1.0],
            text: String::new(),
        }
    }

    #[test]
    fn runs_seconds_script_for_scale() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(1, "hand"));
        host.add_property_script(
            1,
            ScriptField::Scale,
            "'use strict';\nexport function update(value) {\n  let second = new Date().getSeconds();\n  value.x = second / 60;\n  return value;\n}\n",
            json!(null),
            &serde_json::Map::new(),
        );
        let out = host.tick(0.0, 0.016, 0.5, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &[frame(1)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].field, ScriptField::Scale);
        let v = out[0].value.as_array().unwrap();
        assert!(v[0].as_f64().unwrap() <= 1.0);
    }

    #[test]
    fn text_script_returns_string() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(7, "clock"));
        host.add_property_script(
            7,
            ScriptField::Text,
            "export function update() { return 'hello ' + engine.canvasSize.x; }",
            json!("authored"),
            &serde_json::Map::new(),
        );
        let out = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &[frame(7)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, json!("hello 1920"));
    }

    #[test]
    fn this_layer_visible_script() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(3, "holiday"));
        host.add_property_script(
            3,
            ScriptField::Visible,
            "export function update() { const now = new Date(); thisLayer.visible = now.getMonth() === 0; }",
            json!(true),
            &serde_json::Map::new(),
        );
        let out = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &[frame(3)]);
        assert_eq!(out.len(), 1);
        assert!(out[0].value.is_boolean());
    }

    #[test]
    fn script_properties_and_modules() {
        let mut props = serde_json::Map::new();
        props.insert("speed".into(), json!(2.5));
        let mut host = SceneScripts::new(&props).expect("host");
        host.add_layer(&seed(4, "pulse"));
        host.add_property_script(
            4,
            ScriptField::Alpha,
            "import * as WEMath from 'WEMath';\nexport var scriptProperties = createScriptProperties().addSlider({ name: 'speed', value: 0.5 }).finish();\nexport function update() { return WEMath.mix(0, 1, scriptProperties.speed / 2.5); }",
            json!(0.0),
            &serde_json::Map::new(),
        );
        let out = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &[frame(4)]);
        assert_eq!(out.len(), 1);
        assert!((out[0].value.as_f64().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn this_scene_lookup_by_name() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(5, "right-hand"));
        host.add_layer(&seed(6, "flower"));
        host.add_property_script(
            6,
            ScriptField::Text,
            "export function update() { var l = thisScene.getLayer('right-hand'); return l ? l.name : 'missing'; }",
            json!(""),
            &serde_json::Map::new(),
        );
        let out = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &[frame(5), frame(6)]);
        assert_eq!(out[0].value, json!("right-hand"));
    }

    #[test]
    fn binding_script_properties_override_declared_defaults() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(11, "bar"));
        let mut props = serde_json::Map::new();
        props.insert("fill".into(), json!(0.25));
        host.add_property_script(
            11,
            ScriptField::Alpha,
            "export var scriptProperties = createScriptProperties().addSlider({ name: 'fill', value: 1 }).finish();\nexport function update() { return scriptProperties.fill; }",
            json!(1.0),
            &props,
        );
        let out = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &[frame(11)]);
        assert_eq!(out.len(), 1);
        assert!((out[0].value.as_f64().unwrap() - 0.25).abs() < 1e-6);
    }

    #[test]
    fn broken_script_is_disabled_after_error() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(9, "bad"));
        host.add_property_script(
            9,
            ScriptField::Alpha,
            "export function update() { return nope(); }",
            json!(1.0),
            &serde_json::Map::new(),
        );
        let layers = [frame(9)];
        let _ = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &layers);
        let out = host.tick(0.0, 0.016, 0.0, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &layers);
        assert!(out.is_empty());
    }

    /// Compile and tick every SceneScript in the local Wallpaper Engine
    /// Workshop library.  Ignored by default: needs the workshop files.
    #[test]
    #[ignore = "requires the Wallpaper Engine workshop library"]
    fn workshop_scripts_compile_and_tick() {
        let root = std::env::var("WE_WORKSHOP").unwrap_or_else(|_| {
            format!(
                "{}/.local/share/Steam/steamapps/workshop/content/431960",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let mut scenes = 0usize;
        let mut bindings = 0usize;
        let mut failures = 0usize;
        let mut reports: Vec<String> = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&root) else {
            eprintln!("no workshop library at {root}");
            return;
        };
        for dir in dirs.flatten() {
            let pkg = dir.path().join("scene.pkg");
            if !pkg.is_file() {
                continue;
            }
            let Ok(files) = crate::pkg::parse_pkg_file(&pkg) else {
                continue;
            };
            let Some(raw) = files.get("scene.json") else {
                continue;
            };
            let Ok(text) = std::str::from_utf8(raw) else {
                continue;
            };
            let Ok(scene) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if !text.contains("\"script\"") {
                continue;
            }
            if scene.get("objects").and_then(|o| o.as_array()).is_none() {
                continue;
            }
            // Every object in the scene tree, including layer files that are
            // inlined as nested `objects` arrays.
            let mut objs: Vec<Value> = Vec::new();
            fn collect(v: &Value, out: &mut Vec<Value>) {
                match v {
                    Value::Object(map) => {
                        if map.get("id").and_then(|i| i.as_i64()).is_some() {
                            out.push(v.clone());
                        }
                        for value in map.values() {
                            collect(value, out);
                        }
                    }
                    Value::Array(items) => {
                        for item in items {
                            collect(item, out);
                        }
                    }
                    _ => {}
                }
            }
            collect(&scene, &mut objs);
            let id_of = |v: &Value| v.get("id").and_then(|i| i.as_i64());
            let mut ids: Vec<i64> = Vec::new();
            for obj in &objs {
                if let Some(id) = id_of(obj) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            let mut index: std::collections::HashMap<i64, usize> =
                std::collections::HashMap::new();
            for (i, id) in ids.iter().enumerate() {
                index.insert(*id, i);
            }
            let props = serde_json::Map::new();
            let mut host = match SceneScripts::new(&props) {
                Some(h) => h,
                None => break,
            };
            let mut frames = Vec::new();
            for obj in &objs {
                let Some(id) = id_of(obj) else { continue };
                if frames.iter().any(|f: &LayerFrame| f.id == id) {
                    continue;
                }
                let vec = |key: &str, d: [f32; 3]| prop_vec3(obj.get(key).unwrap_or(&Value::Null), d);
                let parent = obj.get("parent").and_then(|p| {
                    p.as_i64()
                        .or_else(|| p.as_str().and_then(|s| s.trim().parse().ok()))
                });
                let _ = &index;
                host.add_layer(&LayerSeed {
                    id,
                    name: obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    kind: "image".into(),
                    visible: true,
                    alpha: 1.0,
                    origin: vec("origin", [0.0; 3]),
                    scale: vec("scale", [1.0; 3]),
                    angles: vec("angles", [0.0; 3]),
                    color: vec("color", [1.0; 3]),
                    size: [100.0, 100.0],
                    align: [0.0, 0.0],
                    text: String::new(),
                    parent,
                });
                frames.push(LayerFrame {
                    id,
                    visible: true,
                    alpha: 1.0,
                    origin: vec("origin", [0.0; 3]),
                    scale: vec("scale", [1.0; 3]),
                    angles: vec("angles", [0.0; 3]),
                    color: vec("color", [1.0; 3]),
                    text: String::new(),
                });
            }
            host.link_parents();
            for obj in &objs {
                let Some(id) = id_of(obj) else { continue };
                for (field, key) in [
                    (ScriptField::Visible, "visible"),
                    (ScriptField::Alpha, "alpha"),
                    (ScriptField::Origin, "origin"),
                    (ScriptField::Scale, "scale"),
                    (ScriptField::Angles, "angles"),
                    (ScriptField::Color, "color"),
                    (ScriptField::Text, "text"),
                ] {
                    let Some(def) = obj.get(key) else { continue };
                    let Some(script) = def.get("script").and_then(|s| s.as_str()) else {
                        continue;
                    };
                    // Particle objects carry scripts on their own tuning
                    // fields (rate, lifetime, ...), which are not layer
                    // properties, so only bind the scene renderer's set.
                    let sp = def
                        .get("scriptproperties")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    let initial = def.get("value").cloned().unwrap_or(Value::Null);
                    host.add_property_script(id, field, script, initial, &sp);
                    bindings += 1;
                }
            }
            scenes += 1;
            for _ in 0..3 {
                let _ = host.tick(1.0, 0.016, 0.5, [0.0, 0.0], [1920.0, 1080.0], [1920.0, 1080.0], &frames);
            }
            let f = host.failures();
            if f > 0 {
                failures += f;
                reports.push(format!(
                    "{}: {f} failing script(s) :: {}",
                    dir.file_name().to_string_lossy(),
                    host.errors().join(" | ")
                ));
            }
        }
        eprintln!(
            "workshop: {scenes} scenes, {bindings} bindings, {failures} failures\n{}",
            reports.join("\n")
        );
        assert!(bindings > 100, "expected the workshop library to be present");
    }
}
