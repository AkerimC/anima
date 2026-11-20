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

use std::collections::{HashMap, HashSet};

use rquickjs::{Context, Function, Runtime};
use serde::Deserialize;
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
    Volume,
    Alignment,
    HorizontalAlign,
    VerticalAlign,
    PointSize,
    Font,
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
            ScriptField::Volume => "volume",
            ScriptField::Alignment => "alignment",
            ScriptField::HorizontalAlign => "horizontalalign",
            ScriptField::VerticalAlign => "verticalalign",
            ScriptField::PointSize => "pointsize",
            ScriptField::Font => "font",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "visible" => Some(Self::Visible),
            "alpha" => Some(Self::Alpha),
            "origin" => Some(Self::Origin),
            "scale" => Some(Self::Scale),
            "angles" => Some(Self::Angles),
            "color" => Some(Self::Color),
            "text" => Some(Self::Text),
            "volume" => Some(Self::Volume),
            "alignment" => Some(Self::Alignment),
            "horizontalalign" => Some(Self::HorizontalAlign),
            "verticalalign" => Some(Self::VerticalAlign),
            "pointsize" => Some(Self::PointSize),
            "font" => Some(Self::Font),
            _ => None,
        }
    }
}

/// Puppet animation layer fields that SceneScript may drive directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnimLayerField {
    Visible,
    Blend,
    Rate,
}

impl AnimLayerField {
    pub fn as_str(self) -> &'static str {
        match self {
            AnimLayerField::Visible => "visible",
            AnimLayerField::Blend => "blend",
            AnimLayerField::Rate => "rate",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "visible" => Some(Self::Visible),
            "blend" => Some(Self::Blend),
            "rate" => Some(Self::Rate),
            _ => None,
        }
    }
}

/// Spritesheet / animated texture metadata exposed to `thisLayer.getTextureAnimation()`.
#[derive(Debug, Clone, Copy)]
pub struct TexAnimSeed {
    pub frames: u32,
    pub duration: f32,
}

/// Video texture metadata exposed to `thisLayer.getVideoTexture()`.
#[derive(Debug, Clone, Copy)]
pub struct VideoTexSeed {
    pub duration: f32,
    pub current_time: f32,
    pub playing: bool,
    pub rate: f32,
}

/// Audio playback metadata exposed to sound layers and `thisLayer.getSound()`.
#[derive(Debug, Clone, Copy)]
pub struct SoundSeed {
    pub volume: f32,
    pub playing: bool,
}

/// Property keyframe track metadata exposed to `thisObject.getAnimation()`.
#[derive(Debug, Clone)]
pub struct PropAnimSeed {
    pub name: String,
    pub fps: f32,
    pub length: f32,
    pub mode: &'static str,
    pub start_paused: bool,
}

impl PropAnimSeed {
    pub fn from_track(t: &super::keytrack::KeyTrack) -> Self {
        Self {
            name: t.name.clone(),
            fps: t.fps,
            length: t.length,
            mode: t.mode_str(),
            start_paused: t.start_paused,
        }
    }
}

/// Puppet animation layer metadata exposed to `thisLayer.getAnimationLayer(...)`
/// and `thisObject` in `animationlayers[]` scripts.
#[derive(Debug, Clone)]
pub struct AnimLayerSeed {
    pub layer_id: i64,
    pub anim_layer_idx: usize,
    pub id: Option<i64>,
    pub name: String,
    pub visible: bool,
    pub blend: f32,
    pub rate: f32,
    pub frame_count: u32,
    pub fps: f32,
    pub duration: f32,
    pub mode: &'static str,
}

/// A layer as the scripts see it (`thisLayer` / `thisScene.getLayer()`).
#[derive(Debug, Clone)]
pub struct LayerSeed {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub visible: bool,
    pub solid: bool,
    pub alpha: f32,
    pub origin: [f32; 3],
    pub scale: [f32; 3],
    pub angles: [f32; 3],
    pub color: [f32; 3],
    pub size: [f32; 2],
    pub align: String,
    pub horizontal_align: String,
    pub vertical_align: String,
    pub point_size: f32,
    pub font: String,
    pub image: String,
    pub text: String,
    pub parent: Option<i64>,
    pub tex_anim: Option<TexAnimSeed>,
    pub video_tex: Option<VideoTexSeed>,
    pub sound: Option<SoundSeed>,
}

/// An effect instance as scripts see it (`thisObject` / `thisLayer.getEffect()`).
#[derive(Debug, Clone)]
pub struct EffectSeed {
    pub layer_id: i64,
    pub effect_idx: usize,
    pub id: Option<i64>,
    pub name: String,
    pub visible: bool,
}

/// Per-frame state of a layer handed to the SceneScript host.
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
    pub video_time: Option<(f32, f32)>,
    pub sound: Option<(f32, bool)>,
}

/// Pointer interaction event types delivered to layer scripts (`cursorEnter`, `cursorClick`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorEventKind {
    Enter,
    Leave,
    Down,
    Up,
    Click,
    Move,
}

impl CursorEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enter => "cursorEnter",
            Self::Leave => "cursorLeave",
            Self::Down => "cursorDown",
            Self::Up => "cursorUp",
            Self::Click => "cursorClick",
            Self::Move => "cursorMove",
        }
    }
}

/// One hit-tested pointer event mapped to world, normalized screen, and layer-local space.
#[derive(Debug, Clone, Copy)]
pub struct CursorEventInput {
    pub layer_id: i64,
    pub kind: CursorEventKind,
    pub world_pos: [f32; 2],
    pub screen_pos: [f32; 2],
    pub local_pos: [f32; 2],
}

/// MPRIS2 media session event delivered to SceneScript `media*Changed` callbacks.
#[derive(Debug, Clone)]
pub enum MediaEventInput {
    Status {
        enabled: bool,
    },
    Playback {
        state: u8,
    },
    Properties {
        title: String,
        artist: String,
        album_title: String,
        album_artist: String,
        sub_title: String,
        genres: String,
        content_type: String,
    },
    Timeline {
        position: f32,
        duration: f32,
    },
    Thumbnail {
        has_thumbnail: bool,
        primary_color: [f32; 3],
        secondary_color: [f32; 3],
        tertiary_color: [f32; 3],
        text_color: [f32; 3],
        high_contrast_color: [f32; 3],
    },
}

impl MediaEventInput {
    fn to_json(&self) -> Value {
        match self {
            Self::Status { enabled } => json!({
                "kind": "mediaStatusChanged",
                "enabled": enabled,
            }),
            Self::Playback { state } => json!({
                "kind": "mediaPlaybackChanged",
                "state": state,
            }),
            Self::Properties {
                title,
                artist,
                album_title,
                album_artist,
                sub_title,
                genres,
                content_type,
            } => json!({
                "kind": "mediaPropertiesChanged",
                "title": title,
                "artist": artist,
                "albumTitle": album_title,
                "albumArtist": album_artist,
                "subTitle": sub_title,
                "genres": genres,
                "contentType": content_type,
            }),
            Self::Timeline { position, duration } => json!({
                "kind": "mediaTimelineChanged",
                "position": position,
                "duration": duration,
            }),
            Self::Thumbnail {
                has_thumbnail,
                primary_color,
                secondary_color,
                tertiary_color,
                text_color,
                high_contrast_color,
            } => json!({
                "kind": "mediaThumbnailChanged",
                "hasThumbnail": has_thumbnail,
                "primaryColor": primary_color,
                "secondaryColor": secondary_color,
                "tertiaryColor": tertiary_color,
                "textColor": text_color,
                "highContrastColor": high_contrast_color,
            }),
        }
    }
}

/// Destination property targeted by a [`ScriptOutput`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptTarget {
    Layer {
        id: i64,
        field: ScriptField,
    },
    LayerSolid {
        id: i64,
    },
    EffectVisible {
        layer_id: i64,
        effect_idx: usize,
    },
    EffectConstant {
        layer_id: i64,
        effect_idx: usize,
        pass_idx: usize,
        key: String,
    },
    LayerAnimControl {
        id: i64,
        field: ScriptField,
    },
    EffectConstAnimControl {
        layer_id: i64,
        effect_idx: usize,
        pass_idx: usize,
        key: String,
    },
    TextureAnimation {
        layer_id: i64,
    },
    VideoTexture {
        layer_id: i64,
    },
    PuppetAnimLayerField {
        layer_id: i64,
        anim_layer_idx: usize,
        field: AnimLayerField,
    },
    PuppetAnimLayerControl {
        layer_id: i64,
        anim_layer_idx: usize,
    },
    InstanceOverride {
        layer_id: i64,
        key: String,
    },
    SoundControl {
        layer_id: i64,
    },
    SceneCreateLayer {
        id: i64,
    },
    SceneDestroyLayer {
        id: i64,
    },
    SceneSortLayer {
        id: i64,
    },
}

/// Result of one scripted property (or side-effect write) for this frame.
#[derive(Debug, Clone)]
pub struct ScriptOutput {
    pub target: ScriptTarget,
    pub value: Value,
}

pub struct SceneScripts {
    #[allow(dead_code)]
    rt: Runtime,
    ctx: Context,
    /// One entry per compiled property/effect binding (aligned 1:1 with JS `scripts`).
    bindings: Vec<Binding>,
    /// Script compile/runtime failures, logged once per binding.
    reported: HashMap<usize, u8>,
    /// First few distinct failure messages, for diagnostics and tests.
    errors: Vec<String>,
    timeouts: usize,
    /// Whether any loaded script called `engine.registerAudioBuffers(...)`.
    uses_audio: bool,
    /// Whether any compiled script defines a `media*Changed` callback.
    uses_media: bool,
    /// Layers that declare at least one `cursor*` event callback.
    cursor_layers: HashSet<i64>,
    /// Whether `finalize_init()` has executed `init()` + startup `applyUserProperties()`.
    initialized: bool,
    /// Latest dirty `localStorage` snapshot emitted by scripts, waiting to be flushed to disk.
    pending_storage: Option<Value>,
}

struct Binding {
    target: ScriptTarget,
    enabled: bool,
}

#[derive(Deserialize)]
struct AddScriptAck {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "usesAudio")]
    uses_audio: bool,
    #[serde(default, rename = "hasCursor")]
    has_cursor: bool,
    #[serde(default, rename = "hasMedia")]
    has_media: bool,
}

#[derive(Deserialize)]
struct HostResponse {
    #[serde(default)]
    errors: Vec<HostError>,
    #[serde(default)]
    outputs: Vec<HostOutput>,
    #[serde(default, rename = "usesAudio")]
    uses_audio: bool,
    #[serde(default)]
    storage: Option<Value>,
}

#[derive(Deserialize)]
struct HostError {
    idx: usize,
    #[serde(default = "default_err_kind")]
    kind: String,
    message: String,
}

fn default_err_kind() -> String {
    "runtime".to_string()
}

#[derive(Deserialize)]
struct HostOutput {
    kind: String,
    #[serde(default)]
    id: i64,
    #[serde(default)]
    field: String,
    #[serde(default, rename = "layerId")]
    layer_id: i64,
    #[serde(default, rename = "effectIdx")]
    effect_idx: usize,
    #[serde(default, rename = "passIdx")]
    pass_idx: usize,
    #[serde(default, rename = "animLayerIdx")]
    anim_layer_idx: usize,
    #[serde(default)]
    key: String,
    value: Value,
}

impl SceneScripts {
    /// Create the QuickJS host.  Returns `None` when the runtime cannot start
    /// (never fatal for rendering — the scene simply keeps authored values).
    pub fn new(user_properties: &serde_json::Map<String, Value>) -> Option<Self> {
        Self::new_with_storage(user_properties, None)
    }

    /// Create the QuickJS host seeded with persisted `localStorage` state.
    pub fn new_with_storage(
        user_properties: &serde_json::Map<String, Value>,
        initial_storage: Option<&Value>,
    ) -> Option<Self> {
        let rt = Runtime::new().ok()?;
        let ctx = Context::full(&rt).ok()?;
        if let Err(err) = ctx.with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(PRELUDE.as_bytes())
        }) {
            log::warn!("scenescript: prelude failed: {err}");
            return None;
        }
        let mut seed_map = serde_json::Map::new();
        seed_map.insert(
            "userProperties".into(),
            Value::Object(user_properties.clone()),
        );
        if let Some(st) = initial_storage {
            seed_map.insert("storage".into(), st.clone());
        }
        let seed = Value::Object(seed_map);
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
            uses_audio: false,
            uses_media: false,
            cursor_layers: HashSet::new(),
            initialized: false,
            pending_storage: None,
        })
    }

    pub fn add_layer(&mut self, seed: &LayerSeed) {
        let mut payload = json!({
            "id": seed.id,
            "name": seed.name,
            "kind": seed.kind,
            "visible": seed.visible,
            "solid": seed.solid,
            "alpha": seed.alpha,
            "origin": seed.origin,
            "scale": seed.scale,
            "angles": seed.angles,
            "color": seed.color,
            "size": seed.size,
            "align": seed.align,
            "horizontalalign": seed.horizontal_align,
            "verticalalign": seed.vertical_align,
            "pointsize": seed.point_size,
            "font": seed.font,
            "image": seed.image,
            "text": seed.text,
            "parent": seed.parent,
        });
        if let Some(obj) = payload.as_object_mut() {
            if let Some(ta) = seed.tex_anim {
                obj.insert(
                    "texAnim".into(),
                    json!({ "frames": ta.frames, "duration": ta.duration }),
                );
            }
            if let Some(vt) = seed.video_tex {
                obj.insert(
                    "videoTex".into(),
                    json!({
                        "duration": vt.duration,
                        "currentTime": vt.current_time,
                        "playing": vt.playing,
                        "rate": vt.rate,
                    }),
                );
            }
            if let Some(snd) = seed.sound {
                obj.insert(
                    "sound".into(),
                    json!({
                        "volume": snd.volume,
                        "playing": snd.playing,
                    }),
                );
            }
        }
        if let Err(err) = self.call_unit("__WE_addLayer", &payload.to_string()) {
            self.report_once(usize::MAX, "layer", &err);
        }
    }

    /// Register an effect instance on an already-added layer so scripts can
    /// query `thisLayer.getEffect(...)` and target `thisObject` in effect scripts.
    pub fn add_effect(&mut self, seed: &EffectSeed) {
        let payload = json!({
            "layerId": seed.layer_id,
            "effectIdx": seed.effect_idx,
            "id": seed.id,
            "name": seed.name,
            "visible": seed.visible,
        });
        if let Err(err) = self.call_unit("__WE_addEffect", &payload.to_string()) {
            self.report_once(usize::MAX, "effect", &err);
        }
    }

    /// Register a puppet animation layer on an already-added layer so scripts
    /// can access `thisLayer.getAnimationLayer(...)` and `animationlayers[]` bindings.
    pub fn add_anim_layer(&mut self, seed: &AnimLayerSeed) {
        let payload = json!({
            "layerId": seed.layer_id,
            "animLayerIdx": seed.anim_layer_idx,
            "id": seed.id,
            "name": seed.name,
            "visible": seed.visible,
            "blend": seed.blend,
            "rate": seed.rate,
            "frameCount": seed.frame_count,
            "fps": seed.fps,
            "duration": seed.duration,
            "mode": seed.mode,
        });
        if let Err(err) = self.call_unit("__WE_addAnimLayer", &payload.to_string()) {
            self.report_once(usize::MAX, "animLayer", &err);
        }
    }

    fn prop_anim_json(pa: Option<&PropAnimSeed>) -> Value {
        match pa {
            Some(a) => json!({
                "name": a.name,
                "fps": a.fps,
                "length": a.length,
                "mode": a.mode,
                "startPaused": a.start_paused,
            }),
            None => Value::Null,
        }
    }

    /// Compile and register one layer property script without running `init()` yet.
    /// Call [`Self::finalize_init`] after all layers, effects and scripts are added.
    pub fn add_property_script(
        &mut self,
        id: i64,
        field: ScriptField,
        script: &str,
        initial: Value,
        script_props: &serde_json::Map<String, Value>,
        prop_anim: Option<&PropAnimSeed>,
    ) -> usize {
        let payload = json!({
            "kind": "layer",
            "id": id,
            "field": field.as_str(),
            "source": script,
            "initial": initial,
            "scriptProperties": script_props,
            "propAnim": Self::prop_anim_json(prop_anim),
        });
        self.register_binding(ScriptTarget::Layer { id, field }, &payload)
    }

    /// Compile and register a script driving `effects[effect_idx].visible`.
    pub fn add_effect_visible_script(
        &mut self,
        layer_id: i64,
        effect_idx: usize,
        script: &str,
        initial: bool,
        script_props: &serde_json::Map<String, Value>,
    ) -> usize {
        let payload = json!({
            "kind": "effect_visible",
            "layerId": layer_id,
            "effectIdx": effect_idx,
            "source": script,
            "initial": initial,
            "scriptProperties": script_props,
        });
        self.register_binding(
            ScriptTarget::EffectVisible {
                layer_id,
                effect_idx,
            },
            &payload,
        )
    }

    /// Compile and register a script driving `effects[effect_idx].passes[pass_idx].constantshadervalues[key]`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_effect_constant_script(
        &mut self,
        layer_id: i64,
        effect_idx: usize,
        pass_idx: usize,
        key: &str,
        script: &str,
        initial: Value,
        script_props: &serde_json::Map<String, Value>,
        prop_anim: Option<&PropAnimSeed>,
    ) -> usize {
        let key_lower = key.to_ascii_lowercase();
        let payload = json!({
            "kind": "effect_const",
            "layerId": layer_id,
            "effectIdx": effect_idx,
            "passIdx": pass_idx,
            "key": key_lower,
            "source": script,
            "initial": initial,
            "scriptProperties": script_props,
            "propAnim": Self::prop_anim_json(prop_anim),
        });
        self.register_binding(
            ScriptTarget::EffectConstant {
                layer_id,
                effect_idx,
                pass_idx,
                key: key_lower,
            },
            &payload,
        )
    }

    /// Compile and register a script driving `animationlayers[anim_layer_idx].{visible,blend,rate}`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_anim_layer_script(
        &mut self,
        layer_id: i64,
        anim_layer_idx: usize,
        field: AnimLayerField,
        script: &str,
        initial: Value,
        script_props: &serde_json::Map<String, Value>,
        prop_anim: Option<&PropAnimSeed>,
    ) -> usize {
        let payload = json!({
            "kind": "anim_layer",
            "layerId": layer_id,
            "animLayerIdx": anim_layer_idx,
            "field": field.as_str(),
            "source": script,
            "initial": initial,
            "scriptProperties": script_props,
            "propAnim": Self::prop_anim_json(prop_anim),
        });
        self.register_binding(
            ScriptTarget::PuppetAnimLayerField {
                layer_id,
                anim_layer_idx,
                field,
            },
            &payload,
        )
    }

    /// Compile and register a script driving `instanceoverride[key]` on a particle layer.
    pub fn add_instance_override_script(
        &mut self,
        layer_id: i64,
        key: &str,
        script: &str,
        initial: Value,
        script_props: &serde_json::Map<String, Value>,
    ) -> usize {
        let key_lower = key.to_ascii_lowercase();
        let payload = json!({
            "kind": "instance_override",
            "layerId": layer_id,
            "key": key_lower,
            "source": script,
            "initial": initial,
            "scriptProperties": script_props,
        });
        self.register_binding(
            ScriptTarget::InstanceOverride {
                layer_id,
                key: key_lower,
            },
            &payload,
        )
    }

    fn register_binding(&mut self, target: ScriptTarget, payload: &Value) -> usize {
        let idx = self.bindings.len();
        match self.call_string("__WE_addScript", &payload.to_string()) {
            Ok(raw) => match serde_json::from_str::<AddScriptAck>(&raw) {
                Ok(ack) => {
                    self.uses_audio |= ack.uses_audio;
                    if ack.ok {
                        self.uses_media |= ack.has_media;
                        if ack.has_cursor {
                            let lid = match &target {
                                ScriptTarget::Layer { id, .. }
                                | ScriptTarget::LayerAnimControl { id, .. }
                                | ScriptTarget::LayerSolid { id }
                                | ScriptTarget::SceneCreateLayer { id }
                                | ScriptTarget::SceneDestroyLayer { id }
                                | ScriptTarget::SceneSortLayer { id } => *id,
                                ScriptTarget::EffectVisible { layer_id, .. }
                                | ScriptTarget::EffectConstant { layer_id, .. }
                                | ScriptTarget::EffectConstAnimControl { layer_id, .. }
                                | ScriptTarget::TextureAnimation { layer_id }
                                | ScriptTarget::VideoTexture { layer_id }
                                | ScriptTarget::PuppetAnimLayerField { layer_id, .. }
                                | ScriptTarget::PuppetAnimLayerControl { layer_id, .. }
                                | ScriptTarget::InstanceOverride { layer_id, .. }
                                | ScriptTarget::SoundControl { layer_id } => *layer_id,
                            };
                            self.cursor_layers.insert(lid);
                        }
                        self.bindings.push(Binding {
                            target,
                            enabled: true,
                        });
                    } else {
                        self.bindings.push(Binding {
                            target,
                            enabled: false,
                        });
                        let err = ack.error.unwrap_or_else(|| "compile failed".into());
                        self.report_once(idx, "compile", &err);
                    }
                }
                Err(_) => {
                    self.bindings.push(Binding {
                        target,
                        enabled: true,
                    });
                }
            },
            Err(err) => {
                self.bindings.push(Binding {
                    target,
                    enabled: false,
                });
                self.report_once(idx, "compile", &err);
            }
        }
        idx
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

    /// Run `init(value)` and startup `applyUserProperties(userProperties)` on
    /// every compiled binding, returning any initial or side-effect outputs.
    pub fn finalize_init(&mut self) -> Vec<ScriptOutput> {
        self.initialized = true;
        if self.bindings.is_empty() {
            return Vec::new();
        }
        match self.call_string("__WE_finalizeInit", "") {
            Ok(raw) => self.parse_host_response(&raw),
            Err(err) => {
                self.report_once(usize::MAX, "init", &err);
                Vec::new()
            }
        }
    }

    /// Deliver live user-property updates to `scriptProperties` and every
    /// script's `applyUserProperties(changed)` handler without resetting VM state.
    pub fn apply_user_properties(
        &mut self,
        changed: &serde_json::Map<String, Value>,
        binding_props: &[(usize, serde_json::Map<String, Value>)],
    ) -> Vec<ScriptOutput> {
        if self.bindings.is_empty() {
            return Vec::new();
        }
        let mut out = if !self.initialized {
            self.finalize_init()
        } else {
            Vec::new()
        };
        let mut overrides = serde_json::Map::with_capacity(binding_props.len());
        for (idx, props) in binding_props {
            overrides.insert(idx.to_string(), Value::Object(props.clone()));
        }
        let payload = json!({
            "changed": changed,
            "bindingProps": overrides,
        });
        match self.call_string("__WE_applyUserProperties", &payload.to_string()) {
            Ok(raw) => out.extend(self.parse_host_response(&raw)),
            Err(err) => self.report_once(usize::MAX, "applyUserProperties", &err),
        }
        out
    }

    /// Whether any loaded script registered an audio spectrum buffer.
    pub fn uses_audio(&self) -> bool {
        self.uses_audio
    }

    /// Whether any compiled script registered a `media*Changed` callback.
    pub fn uses_media(&self) -> bool {
        self.uses_media
    }

    /// Whether any loaded script registered a `cursor*` callback.
    pub fn has_cursor_layers(&self) -> bool {
        !self.cursor_layers.is_empty()
    }

    /// Whether layer `id` has at least one active `cursor*` callback.
    pub fn is_cursor_layer(&self, id: i64) -> bool {
        self.cursor_layers.contains(&id)
    }

    /// Whether `(id, field)` is currently driven by an enabled layer binding.
    pub fn has_layer_binding(&self, id: i64, field: ScriptField) -> bool {
        self.bindings.iter().any(|b| {
            b.enabled && matches!(b.target, ScriptTarget::Layer { id: bid, field: bf } if bid == id && bf == field)
        })
    }

    /// Number of bindings disabled by a compile or runtime failure.
    pub fn failures(&self) -> usize {
        self.bindings.iter().filter(|b| !b.enabled).count()
    }

    /// First few distinct failure messages (one line each).
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Bound `(object id, field)` layer pairs.
    pub fn bound_fields(&self) -> impl Iterator<Item = (i64, ScriptField)> + '_ {
        self.bindings.iter().filter_map(|b| match b.target {
            ScriptTarget::Layer { id, field } => Some((id, field)),
            _ => None,
        })
    }

    /// Run one frame of scripts.  `layers` carries the current authored /
    /// animated state of observable layers, and `spectrum` feeds 64-bin audio
    /// data whenever at least one script called `engine.registerAudioBuffers`.
    #[allow(clippy::too_many_arguments)]
    pub fn tick(
        &mut self,
        runtime: f32,
        frametime: f32,
        time_of_day: f32,
        mouse: [f32; 2],
        canvas: [f32; 2],
        screen: [f32; 2],
        spectrum: Option<&[f32; 64]>,
        layers: &[LayerFrame],
    ) -> Vec<ScriptOutput> {
        self.tick_with_input(
            runtime,
            frametime,
            time_of_day,
            mouse,
            canvas,
            screen,
            spectrum,
            layers,
            [0.0, 0.0],
            false,
            &[],
            &[],
        )
    }

    /// Run one frame of scripts with live pointer state, hit-tested cursor events, and media events.
    #[allow(clippy::too_many_arguments)]
    pub fn tick_with_input(
        &mut self,
        runtime: f32,
        frametime: f32,
        time_of_day: f32,
        mouse: [f32; 2],
        canvas: [f32; 2],
        screen: [f32; 2],
        spectrum: Option<&[f32; 64]>,
        layers: &[LayerFrame],
        mouse_screen: [f32; 2],
        mouse_down: bool,
        cursor_events: &[CursorEventInput],
        media_events: &[MediaEventInput],
    ) -> Vec<ScriptOutput> {
        if self.bindings.is_empty() {
            return Vec::new();
        }
        let mut pre_outputs = if !self.initialized {
            self.finalize_init()
        } else {
            Vec::new()
        };

        let mut layer_json = Vec::with_capacity(layers.len());
        for l in layers {
            let mut item = json!({
                "id": l.id,
                "visible": l.visible,
                "alpha": l.alpha,
                "origin": l.origin,
                "scale": l.scale,
                "angles": l.angles,
                "color": l.color,
                "text": l.text,
            });
            if let Some((ct, dur)) = l.video_time {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("videoTime".into(), json!([ct, dur]));
                }
            }
            if let Some((vol, playing)) = l.sound {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("sound".into(), json!([vol, playing]));
                }
            }
            layer_json.push(item);
        }
        let mut payload = json!({
            "runtime": runtime,
            "frametime": frametime,
            "timeOfDay": time_of_day,
            "mouse": mouse,
            "mouseScreen": mouse_screen,
            "mouseDown": mouse_down,
            "canvas": canvas,
            "screen": screen,
            "layers": layer_json,
        });
        if !cursor_events.is_empty() {
            let ev_json: Vec<Value> = cursor_events
                .iter()
                .map(|ev| {
                    json!({
                        "layerId": ev.layer_id,
                        "kind": ev.kind.as_str(),
                        "worldPos": ev.world_pos,
                        "screenPos": ev.screen_pos,
                        "localPos": ev.local_pos,
                    })
                })
                .collect();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("cursorEvents".to_string(), Value::Array(ev_json));
            }
        }
        if !media_events.is_empty() {
            let ev_json: Vec<Value> = media_events.iter().map(MediaEventInput::to_json).collect();
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("mediaEvents".to_string(), Value::Array(ev_json));
            }
        }
        if self.uses_audio {
            if let Some(spec) = spectrum {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("spectrum".to_string(), json!(spec.as_slice()));
                }
            }
        }

        let raw = match self.call_string("__WE_tick", &payload.to_string()) {
            Ok(raw) => raw,
            Err(err) => {
                self.report_once(usize::MAX, "tick", &err);
                return pre_outputs;
            }
        };
        let mut out = self.parse_host_response(&raw);
        if pre_outputs.is_empty() {
            out
        } else {
            pre_outputs.append(&mut out);
            pre_outputs
        }
    }

    /// Take the latest dirty `localStorage` snapshot emitted by scripts, if any.
    pub fn take_storage_update(&mut self) -> Option<Value> {
        self.pending_storage.take()
    }

    fn parse_host_response(&mut self, raw: &str) -> Vec<ScriptOutput> {
        let Ok(resp) = serde_json::from_str::<HostResponse>(raw) else {
            self.report_once(usize::MAX, "tick-output", "invalid JSON from script host");
            return Vec::new();
        };
        self.uses_audio |= resp.uses_audio;
        if let Some(st) = resp.storage {
            self.pending_storage = Some(st);
        }

        for err in resp.errors {
            if let Some(b) = self.bindings.get(err.idx) {
                if !b.enabled {
                    continue;
                }
            }
            self.report_once(err.idx, &err.kind, &err.message);
            if let Some(b) = self.bindings.get_mut(err.idx) {
                b.enabled = false;
            }
        }

        let mut out = Vec::with_capacity(resp.outputs.len());
        for entry in resp.outputs {
            let target = match entry.kind.as_str() {
                "layer" => {
                    let Some(field) = ScriptField::from_str(&entry.field) else {
                        continue;
                    };
                    ScriptTarget::Layer {
                        id: entry.id,
                        field,
                    }
                }
                "layer_solid" => ScriptTarget::LayerSolid { id: entry.id },
                "effect_visible" => ScriptTarget::EffectVisible {
                    layer_id: entry.layer_id,
                    effect_idx: entry.effect_idx,
                },
                "effect_const" => ScriptTarget::EffectConstant {
                    layer_id: entry.layer_id,
                    effect_idx: entry.effect_idx,
                    pass_idx: entry.pass_idx,
                    key: entry.key.to_ascii_lowercase(),
                },
                "layer_anim_ctrl" => {
                    let Some(field) = ScriptField::from_str(&entry.field) else {
                        continue;
                    };
                    ScriptTarget::LayerAnimControl {
                        id: entry.id,
                        field,
                    }
                }
                "effect_const_anim_ctrl" => ScriptTarget::EffectConstAnimControl {
                    layer_id: entry.layer_id,
                    effect_idx: entry.effect_idx,
                    pass_idx: entry.pass_idx,
                    key: entry.key.to_ascii_lowercase(),
                },
                "tex_anim" => ScriptTarget::TextureAnimation {
                    layer_id: entry.layer_id,
                },
                "video_tex" => ScriptTarget::VideoTexture {
                    layer_id: entry.layer_id,
                },
                "anim_layer_field" => {
                    let Some(field) = AnimLayerField::from_str(&entry.field) else {
                        continue;
                    };
                    ScriptTarget::PuppetAnimLayerField {
                        layer_id: entry.layer_id,
                        anim_layer_idx: entry.anim_layer_idx,
                        field,
                    }
                }
                "anim_layer_ctrl" => ScriptTarget::PuppetAnimLayerControl {
                    layer_id: entry.layer_id,
                    anim_layer_idx: entry.anim_layer_idx,
                },
                "instance_override" => ScriptTarget::InstanceOverride {
                    layer_id: entry.layer_id,
                    key: entry.key.to_ascii_lowercase(),
                },
                "sound_ctrl" => ScriptTarget::SoundControl {
                    layer_id: entry.layer_id,
                },
                "scene_create_layer" => ScriptTarget::SceneCreateLayer { id: entry.id },
                "scene_destroy_layer" => ScriptTarget::SceneDestroyLayer { id: entry.id },
                "scene_sort_layer" => ScriptTarget::SceneSortLayer { id: entry.id },
                _ => continue,
            };
            out.push(ScriptOutput {
                target,
                value: entry.value,
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
            match &b.target {
                ScriptTarget::Layer { id, field }
                | ScriptTarget::LayerAnimControl { id, field } => {
                    log::warn!("scenescript: {} on object {id}: {err}", field.as_str());
                }
                ScriptTarget::LayerSolid { id } => {
                    log::warn!("scenescript: solid on object {id}: {err}");
                }
                ScriptTarget::EffectVisible {
                    layer_id,
                    effect_idx,
                } => {
                    log::warn!(
                        "scenescript: effect[{effect_idx}].visible on object {layer_id}: {err}"
                    );
                }
                ScriptTarget::EffectConstant {
                    layer_id,
                    effect_idx,
                    pass_idx,
                    key,
                }
                | ScriptTarget::EffectConstAnimControl {
                    layer_id,
                    effect_idx,
                    pass_idx,
                    key,
                } => {
                    log::warn!(
                        "scenescript: effect[{effect_idx}].pass[{pass_idx}].{key} on object {layer_id}: {err}"
                    );
                }
                ScriptTarget::TextureAnimation { layer_id } => {
                    log::warn!("scenescript: texAnim on object {layer_id}: {err}");
                }
                ScriptTarget::VideoTexture { layer_id } => {
                    log::warn!("scenescript: videoTex on object {layer_id}: {err}");
                }
                ScriptTarget::PuppetAnimLayerField {
                    layer_id,
                    anim_layer_idx,
                    field,
                } => {
                    log::warn!(
                        "scenescript: animationlayers[{anim_layer_idx}].{} on object {layer_id}: {err}",
                        field.as_str()
                    );
                }
                ScriptTarget::PuppetAnimLayerControl {
                    layer_id,
                    anim_layer_idx,
                } => {
                    log::warn!(
                        "scenescript: animationlayers[{anim_layer_idx}] on object {layer_id}: {err}"
                    );
                }
                ScriptTarget::InstanceOverride { layer_id, key } => {
                    log::warn!("scenescript: instanceoverride.{key} on object {layer_id}: {err}");
                }
                ScriptTarget::SoundControl { layer_id } => {
                    log::warn!("scenescript: sound_ctrl on object {layer_id}: {err}");
                }
                ScriptTarget::SceneCreateLayer { id } => {
                    log::warn!("scenescript: createLayer({id}): {err}");
                }
                ScriptTarget::SceneDestroyLayer { id } => {
                    log::warn!("scenescript: destroyLayer({id}): {err}");
                }
                ScriptTarget::SceneSortLayer { id } => {
                    log::warn!("scenescript: sortLayer({id}): {err}");
                }
            }
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
    if (field === 'alpha' || field === 'volume') return num(v, 1);
    if (field === 'pointsize') return num(v, 32);
    if (
      field === 'text' ||
      field === 'alignment' ||
      field === 'horizontalalign' ||
      field === 'verticalalign' ||
      field === 'font'
    ) {
      return v === undefined || v === null ? '' : String(v);
    }
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
  var ZERO4 = { x: 0, y: 0, z: 0, w: 0 };
  function safe4(v) { return v && typeof v === 'object' ? v : ZERO4; }

  function Vec2(x, y) {
    if (typeof x === 'string') {
      var p = x.trim().split(/\s+/).filter(Boolean);
      if (p.length === 1) {
        var s = num(parseFloat(p[0]), 0);
        this.x = s; this.y = s;
      } else {
        this.x = num(parseFloat(p[0]), 0);
        this.y = num(parseFloat(p[1]), 0);
      }
      return;
    }
    if (x && typeof x === 'object') {
      this.x = num(x.x !== undefined ? x.x : x.r, 0);
      this.y = num(x.y !== undefined ? x.y : (x.g !== undefined ? x.g : y), 0);
      return;
    }
    if (y === undefined && typeof x === 'number') { this.x = num(x, 0); this.y = this.x; return; }
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
  Vec2.prototype.dot = function (o) { o = safe2(o); return this.x * num(o.x, 0) + this.y * num(o.y, 0); };
  Vec2.prototype.length = function () { return Math.hypot(this.x, this.y); };
  Vec2.prototype.lengthSqr = function () { return this.x * this.x + this.y * this.y; };
  Vec2.prototype.normalize = function () { var l = this.length() || 1; return new Vec2(this.x / l, this.y / l); };
  Vec2.prototype.perpendicular = function () { return new Vec2(-this.y, this.x); };
  Vec2.prototype.reflect = function (n) {
    n = safe2(n);
    var nx = num(n.x, 0), ny = num(n.y, 0), d = 2 * (this.x * nx + this.y * ny);
    return new Vec2(this.x - d * nx, this.y - d * ny);
  };
  Vec2.prototype.min = function (o) { o = safe2(o); return new Vec2(Math.min(this.x, num(o.x, 0)), Math.min(this.y, num(o.y, 0))); };
  Vec2.prototype.max = function (o) { o = safe2(o); return new Vec2(Math.max(this.x, num(o.x, 0)), Math.max(this.y, num(o.y, 0))); };
  Vec2.prototype.abs = function () { return new Vec2(Math.abs(this.x), Math.abs(this.y)); };
  Vec2.prototype.floor = function () { return new Vec2(Math.floor(this.x), Math.floor(this.y)); };
  Vec2.prototype.ceil = function () { return new Vec2(Math.ceil(this.x), Math.ceil(this.y)); };
  Vec2.prototype.round = function () { return new Vec2(Math.round(this.x), Math.round(this.y)); };
  Vec2.prototype.smoothStep = function (e0, e1) {
    var a = (e0 && typeof e0 === 'object') ? e0 : { x: num(e0, 0), y: num(e0, 0) };
    var b = (e1 && typeof e1 === 'object') ? e1 : { x: num(e1, 1), y: num(e1, 1) };
    return new Vec2(WEMath.smoothStep(num(a.x, 0), num(b.x, 1), this.x), WEMath.smoothStep(num(a.y, 0), num(b.y, 1), this.y));
  };
  Vec2.prototype.copy = function () { return new Vec2(this.x, this.y); };
  Vec2.prototype.clone = Vec2.prototype.copy;
  Vec2.prototype.toVec3 = function (z) { return new Vec3(this.x, this.y, num(z, 0)); };
  Vec2.prototype.equals = function (o) { return !!o && this.x === o.x && this.y === o.y; };
  Vec2.prototype.toString = function () { return this.x + ' ' + this.y; };

  function Vec3(x, y, z) {
    if (typeof x === 'string') {
      var p = x.trim().split(/\s+/).filter(Boolean);
      if (p.length === 1) {
        var s = num(parseFloat(p[0]), 0);
        this.x = s; this.y = s; this.z = s;
      } else {
        this.x = num(parseFloat(p[0]), 0);
        this.y = num(parseFloat(p[1]), 0);
        this.z = num(parseFloat(p[2]), 0);
      }
      return;
    }
    if (x && typeof x === 'object') {
      this.x = num(x.x !== undefined ? x.x : x.r, 0);
      this.y = num(x.y !== undefined ? x.y : (x.g !== undefined ? x.g : y), 0);
      this.z = num(x.z !== undefined ? x.z : (x.b !== undefined ? x.b : z), 0);
      return;
    }
    if (y === undefined && z === undefined && typeof x === 'number') {
      this.x = num(x, 0); this.y = this.x; this.z = this.x;
      return;
    }
    this.x = num(x, 0); this.y = num(y, 0); this.z = num(z, 0);
  }
  Object.defineProperties(Vec3.prototype, {
    r: { get: function () { return this.x; }, set: function (v) { this.x = num(v, 0); } },
    g: { get: function () { return this.y; }, set: function (v) { this.y = num(v, 0); } },
    b: { get: function () { return this.z; }, set: function (v) { this.z = num(v, 0); } }
  });
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
  Vec3.prototype.dot = function (o) { o = safe3(o); return this.x * num(o.x, 0) + this.y * num(o.y, 0) + this.z * num(o.z, 0); };
  Vec3.prototype.cross = function (o) {
    o = safe3(o);
    return new Vec3(this.y * num(o.z, 0) - this.z * num(o.y, 0), this.z * num(o.x, 0) - this.x * num(o.z, 0), this.x * num(o.y, 0) - this.y * num(o.x, 0));
  };
  Vec3.prototype.length = function () { return Math.sqrt(this.lengthSqr()); };
  Vec3.prototype.lengthSqr = function () { return this.x * this.x + this.y * this.y + this.z * this.z; };
  Vec3.prototype.normalize = function () { var l = this.length() || 1; return new Vec3(this.x / l, this.y / l, this.z / l); };
  Vec3.prototype.reflect = function (n) {
    n = safe3(n);
    var nx = num(n.x, 0), ny = num(n.y, 0), nz = num(n.z, 0);
    var d = 2 * (this.x * nx + this.y * ny + this.z * nz);
    return new Vec3(this.x - d * nx, this.y - d * ny, this.z - d * nz);
  };
  Vec3.prototype.min = function (o) { o = safe3(o); return new Vec3(Math.min(this.x, num(o.x, 0)), Math.min(this.y, num(o.y, 0)), Math.min(this.z, num(o.z, 0))); };
  Vec3.prototype.max = function (o) { o = safe3(o); return new Vec3(Math.max(this.x, num(o.x, 0)), Math.max(this.y, num(o.y, 0)), Math.max(this.z, num(o.z, 0))); };
  Vec3.prototype.abs = function () { return new Vec3(Math.abs(this.x), Math.abs(this.y), Math.abs(this.z)); };
  Vec3.prototype.floor = function () { return new Vec3(Math.floor(this.x), Math.floor(this.y), Math.floor(this.z)); };
  Vec3.prototype.ceil = function () { return new Vec3(Math.ceil(this.x), Math.ceil(this.y), Math.ceil(this.z)); };
  Vec3.prototype.round = function () { return new Vec3(Math.round(this.x), Math.round(this.y), Math.round(this.z)); };
  Vec3.prototype.smoothStep = function (e0, e1) {
    var a = (e0 && typeof e0 === 'object') ? e0 : { x: num(e0, 0), y: num(e0, 0), z: num(e0, 0) };
    var b = (e1 && typeof e1 === 'object') ? e1 : { x: num(e1, 1), y: num(e1, 1), z: num(e1, 1) };
    return new Vec3(
      WEMath.smoothStep(num(a.x, 0), num(b.x, 1), this.x),
      WEMath.smoothStep(num(a.y, 0), num(b.y, 1), this.y),
      WEMath.smoothStep(num(a.z, 0), num(b.z, 1), this.z)
    );
  };
  Vec3.prototype.copy = function () { return new Vec3(this.x, this.y, this.z); };
  Vec3.prototype.clone = Vec3.prototype.copy;
  Vec3.prototype.toVec2 = function () { return new Vec2(this.x, this.y); };
  Vec3.prototype.equals = function (o) { return !!o && this.x === o.x && this.y === o.y && this.z === o.z; };
  Vec3.prototype.toString = function () { return this.x + ' ' + this.y + ' ' + this.z; };

  function Vec4(x, y, z, w) {
    if (typeof x === 'string') {
      var p = x.trim().split(/\s+/).filter(Boolean);
      if (p.length === 1) {
        var s = num(parseFloat(p[0]), 0);
        this.x = s; this.y = s; this.z = s; this.w = s;
      } else {
        this.x = num(parseFloat(p[0]), 0);
        this.y = num(parseFloat(p[1]), 0);
        this.z = num(parseFloat(p[2]), 0);
        this.w = num(parseFloat(p[3]), 0);
      }
      return;
    }
    if (x && typeof x === 'object') {
      this.x = num(x.x !== undefined ? x.x : x.r, 0);
      this.y = num(x.y !== undefined ? x.y : x.g, 0);
      this.z = num(x.z !== undefined ? x.z : x.b, 0);
      this.w = num(x.w !== undefined ? x.w : x.a, 0);
      return;
    }
    if (y === undefined && z === undefined && w === undefined && typeof x === 'number') {
      this.x = num(x, 0); this.y = this.x; this.z = this.x; this.w = this.x;
      return;
    }
    this.x = num(x, 0); this.y = num(y, 0); this.z = num(z, 0); this.w = num(w, 0);
  }
  Object.defineProperties(Vec4.prototype, {
    r: { get: function () { return this.x; }, set: function (v) { this.x = num(v, 0); } },
    g: { get: function () { return this.y; }, set: function (v) { this.y = num(v, 0); } },
    b: { get: function () { return this.z; }, set: function (v) { this.z = num(v, 0); } },
    a: { get: function () { return this.w; }, set: function (v) { this.w = num(v, 0); } }
  });
  Vec4.prototype.add = function (o) { o = safe4(o); return new Vec4(this.x + num(o.x, 0), this.y + num(o.y, 0), this.z + num(o.z, 0), this.w + num(o.w, 0)); };
  Vec4.prototype.subtract = function (o) { o = safe4(o); return new Vec4(this.x - num(o.x, 0), this.y - num(o.y, 0), this.z - num(o.z, 0), this.w - num(o.w, 0)); };
  Vec4.prototype.copy = function () { return new Vec4(this.x, this.y, this.z, this.w); };
  Vec4.prototype.clone = Vec4.prototype.copy;
  Vec4.prototype.multiply = function (o) {
    if (o && typeof o === 'object') return new Vec4(this.x * num(o.x, 1), this.y * num(o.y, 1), this.z * num(o.z, 1), this.w * num(o.w, 1));
    var d = num(o, 1);
    return new Vec4(this.x * d, this.y * d, this.z * d, this.w * d);
  };
  Vec4.prototype.divide = function (o) {
    if (o && typeof o === 'object') return new Vec4(this.x / (num(o.x, 1) || 1), this.y / (num(o.y, 1) || 1), this.z / (num(o.z, 1) || 1), this.w / (num(o.w, 1) || 1));
    var d = num(o, 1) || 1;
    return new Vec4(this.x / d, this.y / d, this.z / d, this.w / d);
  };
  Vec4.prototype.mix = function (o, t) {
    o = safe4(o); t = num(t, 0);
    return new Vec4(
      this.x + (num(o.x, 0) - this.x) * t,
      this.y + (num(o.y, 0) - this.y) * t,
      this.z + (num(o.z, 0) - this.z) * t,
      this.w + (num(o.w, 0) - this.w) * t
    );
  };
  Vec4.prototype.dot = function (o) { o = safe4(o); return this.x * num(o.x, 0) + this.y * num(o.y, 0) + this.z * num(o.z, 0) + this.w * num(o.w, 0); };
  Vec4.prototype.lengthSqr = function () { return this.x * this.x + this.y * this.y + this.z * this.z + this.w * this.w; };
  Vec4.prototype.length = function () { return Math.sqrt(this.lengthSqr()); };
  Vec4.prototype.normalize = function () { var l = this.length() || 1; return new Vec4(this.x / l, this.y / l, this.z / l, this.w / l); };
  Vec4.prototype.equals = function (o) { return !!o && this.x === o.x && this.y === o.y && this.z === o.z && this.w === o.w; };
  Vec4.prototype.toVec3 = function () { return new Vec3(this.x, this.y, this.z); };
  Vec4.prototype.toVec2 = function () { return new Vec2(this.x, this.y); };
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
      var hh, ss, vv;
      if (h && typeof h === 'object') {
        hh = num(h.x !== undefined ? h.x : h.r, 0);
        ss = num(h.y !== undefined ? h.y : h.g, 0);
        vv = num(h.z !== undefined ? h.z : h.b, 0);
      } else {
        hh = num(h, 0); ss = num(s, 0); vv = num(v, 0);
      }
      hh = ((hh % 1) + 1) % 1;
      var i = Math.floor(hh * 6), f = hh * 6 - i;
      var p = vv * (1 - ss), q = vv * (1 - f * ss), t = vv * (1 - (1 - f) * ss);
      var r, g, b;
      switch (i % 6) {
        case 0: r = vv; g = t; b = p; break;
        case 1: r = q; g = vv; b = p; break;
        case 2: r = p; g = vv; b = t; break;
        case 3: r = p; g = q; b = vv; break;
        case 4: r = t; g = p; b = vv; break;
        default: r = vv; g = p; b = q; break;
      }
      return new Vec3(r, g, b);
    },
    rgb2hsv: function (c, g, b) {
      var rx, gy, bz;
      if (c && typeof c === 'object') {
        rx = num(c.x !== undefined ? c.x : c.r, 0);
        gy = num(c.y !== undefined ? c.y : c.g, 0);
        bz = num(c.z !== undefined ? c.z : c.b, 0);
      } else {
        rx = num(c, 0); gy = num(g, 0); bz = num(b, 0);
      }
      var max = Math.max(rx, gy, bz), min = Math.min(rx, gy, bz);
      var d = max - min, h = 0;
      if (d > 0) {
        if (max === rx) h = ((gy - bz) / d) % 6;
        else if (max === gy) h = (bz - rx) / d + 2;
        else h = (rx - gy) / d + 4;
        h /= 6;
      }
      return new Vec3(((h % 1) + 1) % 1, max > 0 ? d / max : 0, max);
    }
  };
  var WEVector = {
    angleVector2: function (a) {
      var rad = num(a, 0) * Math.PI / 180;
      return new Vec2(Math.cos(rad), Math.sin(rad));
    },
    angleVector3: function (a) {
      var v = safe3(a);
      var px = num(v.x, 0) * Math.PI / 180;
      var py = num(v.y, 0) * Math.PI / 180;
      var cp = Math.cos(px), sp = Math.sin(px);
      var cy = Math.cos(py), sy = Math.sin(py);
      return new Vec3(cp * cy, cp * sy, -sp);
    },
    vectorAngle2: function (v) {
      v = safe2(v);
      return Math.atan2(num(v.y, 0), num(v.x, 0)) * 180 / Math.PI;
    }
  };
  globalThis.__WE_MODULES = { WEMath: WEMath, WEColor: WEColor, WEVector: WEVector };

  var userProperties = {};
  var timers = [];
  var nextTimer = 1;

  function removeTimer(handle) {
    if (handle == null) return;
    var id = (typeof handle === 'function' && handle.__timerId !== undefined)
      ? handle.__timerId
      : handle;
    for (var i = 0; i < timers.length; i++) {
      if (timers[i].id === id) {
        timers[i].cancelled = true;
        timers.splice(i, 1);
        return;
      }
    }
  }

  function scheduleTimer(fn, ms, repeating) {
    if (typeof fn !== 'function') return function () {};
    var id = nextTimer++;
    var delay = Math.max(0, num(ms, 0)) / 1000;
    var interval = repeating ? Math.max(0.001, delay) : 0;
    timers.push({
      id: id,
      fn: fn,
      at: globalThis.engine.runtime + (repeating ? interval : delay),
      interval: interval,
      cancelled: false,
      layer: globalThis.thisLayer,
      obj: globalThis.thisObject,
      binding: currentBinding
    });
    function cancel() { removeTimer(id); }
    cancel.__timerId = id;
    return cancel;
  }

  var audioRegistered = false;
  var audioCache = {};
  function audioBuffers(res) {
    audioRegistered = true;
    var n = Math.max(1, Math.min(256, Math.round(num(res, 16))));
    var buf = audioCache[n];
    if (!buf) {
      function zeros() { var a = []; for (var i = 0; i < n; i++) a.push(0); return a; }
      buf = audioCache[n] = { length: n, left: zeros(), right: zeros(), average: zeros() };
    }
    return buf;
  }

  function updateAudioBuffers(spec64) {
    if (!Array.isArray(spec64) || spec64.length === 0) return;
    var srcLen = spec64.length;
    var keys = Object.keys(audioCache);
    for (var k = 0; k < keys.length; k++) {
      var buf = audioCache[keys[k]];
      var n = buf.length;
      if (n === srcLen) {
        for (var i = 0; i < n; i++) {
          var v = num(spec64[i], 0);
          buf.left[i] = v;
          buf.right[i] = v;
          buf.average[i] = v;
        }
      } else if (n === 32 && srcLen === 64) {
        for (var i = 0; i < 32; i++) {
          var b = i * 2;
          var v = (num(spec64[b], 0) + num(spec64[b + 1], 0)) * 0.5;
          buf.left[i] = v; buf.right[i] = v; buf.average[i] = v;
        }
      } else if (n === 16 && srcLen === 64) {
        for (var i = 0; i < 16; i++) {
          var b = i * 4;
          var v = (num(spec64[b], 0) + num(spec64[b + 1], 0) + num(spec64[b + 2], 0) + num(spec64[b + 3], 0)) * 0.25;
          buf.left[i] = v; buf.right[i] = v; buf.average[i] = v;
        }
      } else {
        for (var i = 0; i < n; i++) {
          var start = Math.floor(i * srcLen / n);
          var end = Math.max(start + 1, Math.min(srcLen, Math.floor((i + 1) * srcLen / n)));
          var sum = 0;
          for (var j = start; j < end; j++) sum += num(spec64[j], 0);
          var v = sum / (end - start);
          buf.left[i] = v; buf.right[i] = v; buf.average[i] = v;
        }
      }
    }
  }

  globalThis.engine = {
    runtime: 0,
    frametime: 0,
    timeOfDay: 0,
    canvasSize: new Vec2(1920, 1080),
    screenResolution: new Vec2(1920, 1080),
    mousePosition: new Vec2(0, 0),
    userProperties: userProperties,
    AUDIO_RESOLUTION_16: 16,
    AUDIO_RESOLUTION_32: 32,
    AUDIO_RESOLUTION_64: 64,
    registerAudioBuffers: function (res) { return audioBuffers(res); },
    isScreensaver: function () { return false; },
    registerAsset: function (p) { return p; },
    openUserShortcut: function () { return 0; },
    createAnimation: function () { return animStub(); },
    createEffect: function () { return {}; },
    setTimeout: function (fn, ms) { return scheduleTimer(fn, ms, false); },
    clearTimeout: function (h) { removeTimer(h); },
    setInterval: function (fn, ms) { return scheduleTimer(fn, ms, true); },
    clearInterval: function (h) { removeTimer(h); }
  };
  globalThis.setTimeout = globalThis.engine.setTimeout;
  globalThis.clearTimeout = globalThis.engine.clearTimeout;
  globalThis.setInterval = globalThis.engine.setInterval;
  globalThis.clearInterval = globalThis.engine.clearInterval;

  globalThis.shared = {};

  // Wallpaper Engine persists small values (including Vec2/3/4) with get/set/delete.
  var storage = {};
  var storageDirty = false;
  function serializeStorageVal(v, depth) {
    if (depth > 8 || v === undefined || typeof v === 'function') return undefined;
    if (v === null || typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean') return v;
    if (v instanceof Vec2) return { __weVec: 2, v: [num(v.x, 0), num(v.y, 0)] };
    if (v instanceof Vec3) return { __weVec: 3, v: [num(v.x, 0), num(v.y, 0), num(v.z, 0)] };
    if (v instanceof Vec4) return { __weVec: 4, v: [num(v.x, 0), num(v.y, 0), num(v.z, 0), num(v.w, 0)] };
    if (Array.isArray(v)) {
      var arr = [];
      for (var i = 0; i < v.length; i++) {
        var sv = serializeStorageVal(v[i], depth + 1);
        arr.push(sv === undefined ? null : sv);
      }
      return arr;
    }
    if (typeof v === 'object') {
      var out = {};
      for (var k in v) {
        if (!Object.prototype.hasOwnProperty.call(v, k)) continue;
        var sv2 = serializeStorageVal(v[k], depth + 1);
        if (sv2 !== undefined) out[k] = sv2;
      }
      return out;
    }
    return undefined;
  }
  function deserializeStorageVal(v, depth) {
    if (depth > 8 || v === null || typeof v !== 'object') return v;
    if (Array.isArray(v)) {
      return v.map(function (item) { return deserializeStorageVal(item, depth + 1); });
    }
    if (v.__weVec === 2 && Array.isArray(v.v)) return new Vec2(v.v[0], v.v[1]);
    if (v.__weVec === 3 && Array.isArray(v.v)) return new Vec3(v.v[0], v.v[1], v.v[2]);
    if (v.__weVec === 4 && Array.isArray(v.v)) return new Vec4(v.v[0], v.v[1], v.v[2], v.v[3]);
    var out = {};
    for (var k in v) {
      if (Object.prototype.hasOwnProperty.call(v, k)) {
        out[k] = deserializeStorageVal(v[k], depth + 1);
      }
    }
    return out;
  }
  function cloneStorageOut(v) {
    if (v instanceof Vec2 || v instanceof Vec3 || v instanceof Vec4) return v.copy();
    return v;
  }
  function lsRemove(k) {
    var key = String(k);
    if (Object.prototype.hasOwnProperty.call(storage, key)) {
      delete storage[key];
      storageDirty = true;
    }
  }
  globalThis.localStorage = {
    LOCATION_SCREEN: 0,
    LOCATION_GLOBAL: 1,
    get: function (k) {
      var key = String(k);
      if (!Object.prototype.hasOwnProperty.call(storage, key)) return undefined;
      return cloneStorageOut(storage[key]);
    },
    set: function (k, v) {
      storage[String(k)] = v instanceof Vec2 || v instanceof Vec3 || v instanceof Vec4 ? v.copy() : v;
      storageDirty = true;
    },
    delete: lsRemove,
    remove: lsRemove,
    clear: function () {
      storage = {};
      storageDirty = true;
    }
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
    if (v && typeof v === 'object') return new Vec3(v.x !== undefined ? v.x : v.r, v.y !== undefined ? v.y : v.g, v.z !== undefined ? v.z : v.b);
    if (typeof v === 'string') {
      var p = v.trim().split(/\s+/).map(parseFloat);
      return new Vec3(p[0], p[1], p[2]);
    }
    return new Vec3(0, 0, 0);
  }
  function parseScaleVec(v) {
    if (typeof v === 'number') {
      var n = num(v, 1);
      return new Vec3(n, n, n);
    }
    if (Array.isArray(v)) return new Vec3(num(v[0], 1), num(v[1], 1), num(v[2], 1));
    if (v && typeof v === 'object') {
      return new Vec3(
        num(v.x !== undefined ? v.x : v.r, 1),
        num(v.y !== undefined ? v.y : v.g, 1),
        num(v.z !== undefined ? v.z : v.b, 1)
      );
    }
    if (typeof v === 'string') {
      var p = v.trim().split(/\s+/).filter(Boolean).map(parseFloat);
      if (p.length === 1) {
        var s = num(p[0], 1);
        return new Vec3(s, s, s);
      }
      if (p.length >= 2) {
        return new Vec3(num(p[0], 1), num(p[1], 1), num(p[2], 1));
      }
    }
    return new Vec3(1, 1, 1);
  }
  function toArr3(v) {
    if (Array.isArray(v)) return [num(v[0], 0), num(v[1], 0), num(v[2], 0)];
    if (v && typeof v === 'object') return [num(v.x !== undefined ? v.x : v.r, 0), num(v.y !== undefined ? v.y : v.g, 0), num(v.z !== undefined ? v.z : v.b, 0)];
    var n = typeof v === 'number' ? num(v, 0) : 0;
    return [n, n, n];
  }
  function vec3Eq(a, b) {
    return a[0] === b[0] && a[1] === b[1] && a[2] === b[2];
  }

  function parseConstInitial(v) {
    if (typeof v === 'boolean') return { value: v, kind: 'bool' };
    if (typeof v === 'number') return { value: num(v, 0), kind: 'number' };
    if (Array.isArray(v)) {
      if (v.length >= 4) return { value: new Vec4(v[0], v[1], v[2], v[3]), kind: 'vec4' };
      if (v.length === 3) return { value: new Vec3(v[0], v[1], v[2]), kind: 'vec3' };
      if (v.length === 2) return { value: new Vec2(v[0], v[1]), kind: 'vec2' };
      if (v.length === 1) return { value: num(v[0], 0), kind: 'number' };
      return { value: 0, kind: 'number' };
    }
    if (typeof v === 'string') {
      var parts = v.trim().split(/\s+/).filter(Boolean);
      var nums = [];
      var allNum = parts.length > 0;
      for (var i = 0; i < parts.length; i++) {
        var f = Number(parts[i]);
        if (!isFinite(f)) { allNum = false; break; }
        nums.push(f);
      }
      if (allNum) {
        if (nums.length === 1) return { value: nums[0], kind: 'number' };
        if (nums.length === 2) return { value: new Vec2(nums[0], nums[1]), kind: 'vec2' };
        if (nums.length === 3) return { value: new Vec3(nums[0], nums[1], nums[2]), kind: 'vec3' };
        if (nums.length >= 4) return { value: new Vec4(nums[0], nums[1], nums[2], nums[3]), kind: 'vec4' };
      }
      return { value: v, kind: 'string' };
    }
    if (v && typeof v === 'object') {
      if (v.w !== undefined || v.a !== undefined) return { value: new Vec4(v), kind: 'vec4' };
      if (v.z !== undefined || v.b !== undefined) return { value: new Vec3(v), kind: 'vec3' };
      if (v.x !== undefined || v.r !== undefined) return { value: new Vec2(v), kind: 'vec2' };
    }
    return { value: 0, kind: 'number' };
  }

  function copyConstValue(v) {
    if (v instanceof Vec2 || v instanceof Vec3 || v instanceof Vec4) return v.copy();
    return v;
  }

  function normalizeConstValue(res, kind) {
    if (res instanceof Vec2) return res.copy();
    if (res instanceof Vec3) return res.copy();
    if (res instanceof Vec4) return res.copy();
    if (res && typeof res === 'object') {
      if (Array.isArray(res)) {
        if (res.length >= 4) return new Vec4(res[0], res[1], res[2], res[3]);
        if (res.length === 3) return new Vec3(res[0], res[1], res[2]);
        if (res.length === 2) return new Vec2(res[0], res[1]);
        if (res.length === 1) return num(res[0], 0);
        return 0;
      }
      if (res.w !== undefined || res.a !== undefined) return new Vec4(res);
      if (res.z !== undefined || res.b !== undefined) return new Vec3(res);
      if (res.x !== undefined || res.r !== undefined) return new Vec2(res);
    }
    if (kind === 'bool') return !!res;
    if (typeof res === 'boolean') return res;
    if (typeof res === 'number') return isFinite(res) ? res : 0;
    if (typeof res === 'string') return parseConstInitial(res).value;
    return 0;
  }

  function serializeConstValue(v) {
    if (v instanceof Vec2) return [num(v.x, 0), num(v.y, 0)];
    if (v instanceof Vec3) return [num(v.x, 0), num(v.y, 0), num(v.z, 0)];
    if (v instanceof Vec4) return [num(v.x, 0), num(v.y, 0), num(v.z, 0), num(v.w, 0)];
    if (v && typeof v === 'object') {
      if (v.w !== undefined) return [num(v.x, 0), num(v.y, 0), num(v.z, 0), num(v.w, 0)];
      if (v.z !== undefined) return [num(v.x, 0), num(v.y, 0), num(v.z, 0)];
      if (v.x !== undefined) return [num(v.x, 0), num(v.y, 0)];
    }
    if (typeof v === 'number') return isFinite(v) ? v : 0;
    return v;
  }

  var layers = {};
  var layerOrder = [];
  var nextDynLayerId = -1000;
  var dynLayerCount = 0;
  var pendingSceneOps = [];
  var createdThisTick = {};
  var allEffects = [];
  var allAnimLayers = [];
  var allKeyAnims = [];
  var allTexAnims = [];
  var allVideoTexs = [];
  var scripts = [];
  var currentBinding = null;

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

  function addEndedHandler(list, fn) {
    if (typeof fn === 'function' && list.indexOf(fn) === -1) list.push(fn);
  }
  function removeEndedHandler(list, fn) {
    var i = list.indexOf(fn);
    if (i >= 0) list.splice(i, 1);
  }
  function fireEndedHandlers(a) {
    if (!a.__endedCallbacks || !a.__endedCallbacks.length) return;
    var prevScene = globalThis.thisScene;
    var prevLayer = globalThis.thisLayer;
    var prevObj = globalThis.thisObject;
    var cbs = a.__endedCallbacks.slice();
    if (a.__layer !== undefined) globalThis.thisLayer = a.__layer;
    if (a.__obj !== undefined) globalThis.thisObject = a.__obj;
    globalThis.thisScene = scene;
    for (var i = 0; i < cbs.length; i++) {
      try { cbs[i](); } catch (_err) {}
    }
    globalThis.thisScene = prevScene;
    globalThis.thisLayer = prevLayer;
    globalThis.thisObject = prevObj;
  }

  function animStub() {
    var a = {
      rate: 1, currentFrame: 0, frameCount: 0, duration: 0, __playing: false,
      __endedCallbacks: [],
      play: function () { a.__playing = true; },
      pause: function () { a.__playing = false; },
      stop: function () { a.__playing = false; a.currentFrame = 0; },
      isPlaying: function () { return a.__playing; },
      setFrame: function (f) { a.currentFrame = num(f, 0); },
      getFrame: function () { return a.currentFrame; },
      setPlaybackRate: function (r) { a.rate = num(r, 1); },
      getPlaybackRate: function () { return a.rate; },
      addEndedCallback: function (fn) { addEndedHandler(a.__endedCallbacks, fn); },
      removeEndedCallback: function (fn) { removeEndedHandler(a.__endedCallbacks, fn); },
      getAnimation: function () { return a; }
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
    Object.defineProperty(v, 'rate', {
      get: function () { return v.playbackRate; },
      set: function (r) { v.setPlaybackRate(r); },
      enumerable: true
    });
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

  function makeKeyAnim(meta, ctrlKind, ctrlTarget) {
    var fps = Math.max(1, num(meta.fps, 30));
    var frameCount = num(meta.frameCount !== undefined ? meta.frameCount : meta.length, 60);
    var maxFrame = Math.max(0, num(meta.maxFrame !== undefined ? meta.maxFrame : frameCount, 0));
    var duration = meta.duration !== undefined && num(meta.duration, 0) > 0
      ? num(meta.duration, 0)
      : (maxFrame / fps);
    var mode = meta.mode || 'loop';
    var a = {
      name: meta.name || '',
      fps: fps,
      frameCount: frameCount,
      duration: duration,
      mode: mode,
      __maxFrame: maxFrame,
      __playing: !meta.startPaused,
      __rate: meta.rate !== undefined ? num(meta.rate, 1) : 1,
      __frame: 0,
      __mirrorReverse: false,
      __dirty: false,
      __endedCallbacks: [],
      __ctrlKind: ctrlKind,
      __ctrlTarget: ctrlTarget,
      __layer: meta.layer,
      __obj: meta.obj,
      play: function () {
        if (a.mode === 'single') {
          if (a.__rate >= 0 && a.__frame >= a.__maxFrame - 1e-4) a.__frame = 0;
          else if (a.__rate < 0 && a.__frame <= 1e-4) a.__frame = a.__maxFrame;
        }
        a.__playing = true;
        a.__dirty = true;
      },
      pause: function () { a.__playing = false; a.__dirty = true; },
      stop: function () {
        a.__playing = false;
        a.__frame = 0;
        a.__mirrorReverse = false;
        a.__dirty = true;
      },
      isPlaying: function () { return a.__playing; },
      setFrame: function (f) {
        a.__frame = Math.max(0, Math.min(a.__maxFrame, num(f, 0)));
        a.__mirrorReverse = false;
        a.__dirty = true;
      },
      getFrame: function () { return a.__frame; },
      setPlaybackRate: function (r) {
        var nr = num(r, 1);
        if (isFinite(nr)) { a.__rate = nr; a.__dirty = true; }
      },
      getPlaybackRate: function () { return a.__rate; },
      addEndedCallback: function (fn) { addEndedHandler(a.__endedCallbacks, fn); },
      removeEndedCallback: function (fn) { removeEndedHandler(a.__endedCallbacks, fn); },
      getAnimation: function () { return a; }
    };
    Object.defineProperty(a, 'rate', {
      get: function () { return a.__rate; },
      set: function (r) { a.setPlaybackRate(r); },
      enumerable: true
    });
    Object.defineProperty(a, 'currentFrame', {
      get: function () { return a.__frame; },
      set: function (f) { a.setFrame(f); },
      enumerable: true
    });
    allKeyAnims.push(a);
    return a;
  }

  function advanceKeyAnim(a, dt) {
    if (!a.__playing || dt <= 0 || a.__rate === 0 || a.__maxFrame <= 0) return;
    var delta = dt * a.fps * a.__rate;
    var len = a.__maxFrame;
    if (a.mode === 'single') {
      var next = a.__frame + delta;
      if (delta >= 0 && next >= len) {
        a.__frame = len;
        a.__playing = false;
        fireEndedHandlers(a);
      } else if (delta < 0 && next <= 0) {
        a.__frame = 0;
        a.__playing = false;
        fireEndedHandlers(a);
      } else {
        a.__frame = next;
      }
    } else if (a.mode === 'mirror') {
      var period = Math.max(len * 2, 1e-4);
      var phase = a.__mirrorReverse ? (period - a.__frame) : a.__frame;
      var m = ((phase + delta) % period + period) % period;
      if (m <= len) {
        a.__frame = m;
        a.__mirrorReverse = false;
      } else {
        a.__frame = period - m;
        a.__mirrorReverse = true;
      }
    } else {
      a.__frame = ((a.__frame + delta) % len + len) % len;
    }
  }

  function makeTexAnim(layerId, meta) {
    var frames = Math.max(1, Math.round(num(meta.frames, 1)));
    var dur = Math.max(0.01, num(meta.duration, 1));
    var a = {
      frameCount: frames,
      duration: dur,
      __layerId: layerId,
      __fps: frames / dur,
      __playing: true,
      __rate: 1,
      __frame: 0,
      __controlled: false,
      __dirty: false,
      __lastEmittedFrame: -1,
      __endedCallbacks: [],
      play: function () { a.__playing = true; a.__controlled = true; a.__dirty = true; },
      pause: function () { a.__playing = false; a.__controlled = true; a.__dirty = true; },
      stop: function () {
        a.__playing = false;
        a.__frame = 0;
        a.__controlled = true;
        a.__dirty = true;
      },
      isPlaying: function () { return a.__playing; },
      setFrame: function (f) {
        var fc = Math.max(1, a.frameCount);
        var nf = Math.max(0, Math.min(fc - 1, Math.floor(num(f, 0))));
        a.__frame = nf;
        a.__controlled = true;
        a.__dirty = true;
      },
      getFrame: function () { return Math.floor(a.__frame); },
      setPlaybackRate: function (r) {
        var nr = num(r, 1);
        a.__controlled = true;
        if (nr !== a.__rate) { a.__rate = nr; a.__dirty = true; }
      },
      getPlaybackRate: function () { return a.__rate; },
      addEndedCallback: function (fn) { addEndedHandler(a.__endedCallbacks, fn); },
      removeEndedCallback: function (fn) { removeEndedHandler(a.__endedCallbacks, fn); },
      getAnimation: function () { return a; }
    };
    Object.defineProperty(a, 'rate', {
      get: function () { return a.__rate; },
      set: function (r) { a.setPlaybackRate(r); },
      enumerable: true
    });
    Object.defineProperty(a, 'currentFrame', {
      get: function () { return a.getFrame(); },
      set: function (f) { a.setFrame(f); },
      enumerable: true
    });
    allTexAnims.push(a);
    return a;
  }

  function advanceTexAnim(a, dt, runtime) {
    var fc = Math.max(1, a.frameCount);
    if (a.__controlled) {
      if (a.__playing && a.__rate !== 0 && dt > 0) {
        a.__frame = ((a.__frame + dt * a.__fps * a.__rate) % fc + fc) % fc;
      }
    } else if (fc > 1) {
      var u = ((runtime / a.duration) % 1 + 1) % 1;
      a.__frame = Math.min(fc - 1, Math.floor(u * fc));
    }
  }

  function makeVideoTex(layerId, meta) {
    var v = {
      currentTime: num(meta.currentTime, 0),
      duration: Math.max(0, num(meta.duration, 0)),
      playbackRate: num(meta.rate, 1),
      __playing: meta.playing !== false,
      muted: true,
      volume: 0,
      __layerId: layerId,
      __real: true,
      __dirty: false,
      __pendingSeek: null,
      play: function () {
        if (!v.__playing) { v.__playing = true; v.__dirty = true; }
      },
      pause: function () {
        if (v.__playing) { v.__playing = false; v.__dirty = true; }
      },
      stop: function () {
        v.__playing = false;
        v.currentTime = 0;
        v.__pendingSeek = 0;
        v.__dirty = true;
      },
      isPlaying: function () { return v.__playing; },
      setCurrentTime: function (t) {
        var nt = Math.max(0, num(t, 0));
        v.currentTime = nt;
        v.__pendingSeek = nt;
        v.__dirty = true;
      },
      getCurrentTime: function () { return v.currentTime; },
      setPlaybackRate: function (r) {
        var nr = num(r, 1);
        if (Math.abs(nr - v.playbackRate) > 1e-4) {
          v.playbackRate = nr;
          v.__dirty = true;
        }
      },
      getPlaybackRate: function () { return v.playbackRate; },
      setMuted: function (m) { v.muted = !!m; },
      isMuted: function () { return v.muted; }
    };
    Object.defineProperty(v, 'rate', {
      get: function () { return v.playbackRate; },
      set: function (r) { v.setPlaybackRate(r); },
      enumerable: true
    });
    allVideoTexs.push(v);
    return v;
  }

  function findNamedAnim(list, ref) {
    var s = String(ref || '');
    for (var i = 0; i < list.length; i++) {
      if (list[i].name === s) return list[i];
    }
    var sl = s.toLowerCase();
    for (var i = 0; i < list.length; i++) {
      if (list[i].name.toLowerCase() === sl) return list[i];
    }
    return null;
  }

  function computeWorldTransform(layer) {
    var chain = [];
    var cur = layer;
    for (var guard = 0; cur && guard < 32; guard++) {
      chain.push(cur);
      cur = cur.__parent;
    }
    var a = 1, b = 0, c = 0, d = 1, tx = 0, ty = 0;
    var sz = 1, tz = 0;
    for (var i = chain.length - 1; i >= 0; i--) {
      var node = chain[i];
      var o = safe3(node.origin);
      var sc = safe3(node.scale);
      var ang = safe3(node.angles);
      var ox = num(o.x, 0), oy = num(o.y, 0), oz = num(o.z, 0);
      var sx = num(sc.x, 1), sy = num(sc.y, 1), nodeSz = num(sc.z, 1);
      var rad = num(ang.z, 0) * Math.PI / 180;
      var cs = Math.cos(rad), sn = Math.sin(rad);
      var la = cs * sx;
      var lb = sn * sx;
      var lc = -sn * sy;
      var ld = cs * sy;
      var ntx = a * ox + c * oy + tx;
      var nty = b * ox + d * oy + ty;
      var na = a * la + c * lb;
      var nb = b * la + d * lb;
      var nc = a * lc + c * ld;
      var nd = b * lc + d * ld;
      a = na; b = nb; c = nc; d = nd; tx = ntx; ty = nty;
      tz += sz * oz;
      sz *= nodeSz;
    }
    return {
      m: [
        a, b, 0, 0,
        c, d, 0, 0,
        0, 0, sz, 0,
        tx, ty, tz, 1
      ]
    };
  }

  function makeLayer(id, name) {
    var _size = new Vec2(0, 0);
    var l = {
      id: id, name: name || '', visible: true, solid: true, alpha: 1, volume: 1,
      origin: new Vec3(0, 0, 0), scale: new Vec3(1, 1, 1), angles: new Vec3(0, 0, 0),
      color: new Vec3(1, 1, 1),
      alignment: 'center', horizontalalign: 'center', verticalalign: 'center',
      pointsize: 32, font: '', image: '',
      text: '', cursorDetected: false, clicked: false, kind: '',
      _origOrigin: new Vec3(0, 0, 0),
      _origScale: new Vec3(1, 1, 1),
      _origAngles: new Vec3(0, 0, 0),
      _origSize: new Vec2(0, 0),
      _origColor: new Vec3(1, 1, 1),
      _origAlpha: 1,
      __parent: null, __tex: null, __video: null, __layerFile: null,
      __effects: {}, __effectList: [],
      __animLayers: {}, __animLayerList: [],
      __propAnims: [],
      __isSound: false, __playing: false, __soundAction: null,
      getParent: function () { return l.__parent || null; },
      getEffect: function (ref) {
        if (typeof ref === 'number') return l.__effects[String(ref)] || l.__effectList[ref] || null;
        return findNamedAnim(l.__effectList, ref);
      },
      getEffectCount: function () { return l.__effectList.length; },
      getAnimationLayer: function (ref) {
        if (typeof ref === 'number') return l.__animLayers[String(ref)] || l.__animLayerList[ref] || null;
        return findNamedAnim(l.__animLayerList, ref);
      },
      getAnimationLayerCount: function () { return l.__animLayerList.length; },
      getTextureAnimation: function () {
        if (!l.__tex) { l.__tex = animStub(); l.__tex.frameCount = 1; }
        return l.__tex;
      },
      getVideoTexture: function () {
        if (!l.__video) l.__video = videoStub();
        return l.__video;
      },
      getAnimation: function (ref) {
        if (ref === undefined && currentBinding && currentBinding.propAnim && currentBinding.layer === l) {
          return currentBinding.propAnim;
        }
        if (ref !== undefined) {
          return findNamedAnim(l.__propAnims, ref) || l.getAnimationLayer(ref) || null;
        }
        return l.__propAnims[0] || l.__animLayerList[0] || l.getTextureAnimation();
      },
      getSound: function () {
        if (!l.__isSound) return soundStub();
        var s = {
          play: function () { l.play(); },
          pause: function () { l.pause(); },
          stop: function () { l.stop(); },
          isPlaying: function () { return l.isPlaying(); },
          setVolume: function (v) { l.volume = num(v, 1); },
          getVolume: function () { return l.volume; },
          setMuted: function () {},
          isMuted: function () { return false; },
          getCurrentTime: function () { return 0; },
          setCurrentTime: function () {}
        };
        Object.defineProperty(s, 'volume', {
          get: function () { return l.volume; },
          set: function (v) { l.volume = num(v, 1); },
          enumerable: true
        });
        return s;
      },
      getLayerFile: function () { return l.__layerFile; },
      getTransformMatrix: function () { return computeWorldTransform(l); },
      isPlaying: function () { return !!l.__playing; },
      play: function () {
        if (l.__isSound) { l.__playing = true; l.__soundAction = 'play'; }
      },
      pause: function () {
        if (l.__isSound) { l.__playing = false; l.__soundAction = 'pause'; }
      },
      stop: function () {
        if (l.__isSound) { l.__playing = false; l.__soundAction = 'stop'; }
      }
    };
    Object.defineProperty(l, 'size', {
      get: function () { return _size.copy(); },
      set: function (v) { _size = new Vec2(v); },
      enumerable: true
    });
    Object.defineProperty(l, 'originalOrigin', { get: function () { return l._origOrigin.copy(); }, enumerable: true });
    Object.defineProperty(l, 'originalScale', { get: function () { return l._origScale.copy(); }, enumerable: true });
    Object.defineProperty(l, 'originalAngles', { get: function () { return l._origAngles.copy(); }, enumerable: true });
    Object.defineProperty(l, 'originalSize', { get: function () { return l._origSize.copy(); }, enumerable: true });
    Object.defineProperty(l, 'originalColor', { get: function () { return l._origColor.copy(); }, enumerable: true });
    Object.defineProperty(l, 'originalAlpha', { get: function () { return l._origAlpha; }, enumerable: true });
    return l;
  }

  var scene = {
    getLayer: function (ref) { return layerByRef(ref); },
    enumerateLayers: function () { return layerOrder.map(function (id) { return layers[id]; }); },
    getLayerIndex: function (l) { var i = layerOrder.indexOf(String(l && l.id)); return i < 0 ? -1 : i; },
    sortLayer: function (ref, targetIdx) {
      var l = layerByRef(ref);
      if (!l || layers[String(l.id)] !== l) return false;
      var idStr = String(l.id);
      var from = layerOrder.indexOf(idStr);
      if (from < 0) return false;
      var to = Math.max(0, Math.min(layerOrder.length - 1, Math.floor(num(targetIdx, from))));
      if (from !== to) {
        layerOrder.splice(from, 1);
        layerOrder.splice(to, 0, idStr);
        pendingSceneOps.push({ kind: 'scene_sort_layer', id: l.id, value: to });
      }
      return true;
    },
    destroyLayer: function (ref) {
      var l = layerByRef(ref);
      if (!l || layers[String(l.id)] !== l) return false;
      var idStr = String(l.id);
      delete layers[idStr];
      delete createdThisTick[idStr];
      var pos = layerOrder.indexOf(idStr);
      if (pos >= 0) layerOrder.splice(pos, 1);
      if (l.id <= -1000 && dynLayerCount > 0) dynLayerCount--;
      pendingSceneOps.push({ kind: 'scene_destroy_layer', id: l.id, value: null });
      return true;
    },
    createLayer: function (cfg) {
      cfg = cfg && typeof cfg === 'object' ? cfg : {};
      if (dynLayerCount >= 256) {
        return makeLayer(-1, cfg.name ? String(cfg.name) : '');
      }
      var dynId = nextDynLayerId--;
      dynLayerCount++;
      var idStr = String(dynId);
      var l = makeLayer(dynId, cfg.name ? String(cfg.name) : ('dyn_' + (-dynId)));
      l.kind = cfg.type ? String(cfg.type) : (cfg.kind ? String(cfg.kind) : '');
      l.image = cfg.image ? String(cfg.image) : '';
      l.text = cfg.text !== undefined && cfg.text !== null ? String(cfg.text) : '';
      l.color = cfg.color !== undefined ? parseVec(cfg.color) : new Vec3(1, 1, 1);
      l.alpha = cfg.alpha !== undefined ? num(cfg.alpha, 1) : 1;
      l.visible = cfg.visible !== false;
      l.solid = cfg.solid !== false;
      l.origin = cfg.origin !== undefined ? parseVec(cfg.origin) : new Vec3(0, 0, 0);
      l.scale = cfg.scale !== undefined ? parseScaleVec(cfg.scale) : new Vec3(1, 1, 1);
      l.angles = cfg.angles !== undefined ? parseVec(cfg.angles) : new Vec3(0, 0, 0);
      var sv = cfg.size !== undefined ? parseVec(cfg.size) : new Vec3(0, 0, 0);
      l.size = new Vec2(sv.x, sv.y);
      l.alignment = cfg.alignment ? String(cfg.alignment) : 'center';
      l.horizontalalign = cfg.horizontalalign ? String(cfg.horizontalalign) : 'center';
      l.verticalalign = cfg.verticalalign ? String(cfg.verticalalign) : 'center';
      l.pointsize = cfg.pointsize !== undefined ? num(cfg.pointsize, 32) : 32;
      l.font = cfg.font ? String(cfg.font) : '';
      var pd = cfg.parallaxDepth !== undefined ? parseVec(cfg.parallaxDepth) : new Vec3(0, 0, 0);
      var parentLayer = cfg.parent !== undefined ? layerByRef(cfg.parent) : null;
      var parentId = parentLayer && typeof parentLayer.id === 'number' && layers[String(parentLayer.id)] === parentLayer
        ? parentLayer.id
        : null;
      if (parentId !== null) {
        l.__parentId = parentId;
        l.__parent = parentLayer;
      }
      l._origOrigin = l.origin.copy();
      l._origScale = l.scale.copy();
      l._origAngles = l.angles.copy();
      l._origSize = l.size.copy();
      l._origColor = l.color.copy();
      l._origAlpha = l.alpha;
      layers[idStr] = l;
      layerOrder.push(idStr);
      createdThisTick[idStr] = true;
      pendingSceneOps.push({
        kind: 'scene_create_layer',
        id: dynId,
        value: {
          name: l.name,
          kind: l.kind,
          image: l.image,
          text: l.text,
          color: toArr3(l.color),
          alpha: l.alpha,
          visible: l.visible,
          solid: l.solid,
          origin: toArr3(l.origin),
          scale: toArr3(l.scale),
          angles: toArr3(l.angles),
          size: [l.size.x, l.size.y],
          alignment: l.alignment,
          horizontalalign: l.horizontalalign,
          verticalalign: l.verticalalign,
          pointsize: l.pointsize,
          font: l.font,
          parallaxDepth: [pd.x, pd.y],
          parent: parentId
        }
      });
      return l;
    },
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
        alignment: l.alignment || 'center',
        horizontalalign: l.horizontalalign || 'center',
        verticalalign: l.verticalalign || 'center',
        pointsize: l.pointsize,
        font: l.font || ''
      };
    },
    getAnimation: animStub,
    getTextureAnimation: animStub
  };

  globalThis.thisScene = scene;
  globalThis.thisLayer = null;

  function coerceScriptProp(type, rawVal) {
    if (type === 'slider') return num(rawVal, 0);
    if (type === 'checkbox') return !!rawVal;
    if (type === 'color' || type === 'direction') return parseVec(rawVal);
    if (type === 'text' || type === 'file' || type === 'font') return String(rawVal == null ? '' : rawVal);
    return rawVal;
  }

  globalThis.createScriptProperties = function () {
    var props = {};
    var types = {};
    var override = globalThis.__WE_binding_props || {};
    function read(name, fallback) {
      var v = override[name];
      if (v === undefined) v = userProperties[name];
      return (v === undefined || v === null) ? fallback : v;
    }
    var api = {
      addSlider: function (o) { types[o.name] = 'slider'; props[o.name] = num(read(o.name, o.value), 0); return api; },
      addCheckbox: function (o) { types[o.name] = 'checkbox'; props[o.name] = !!read(o.name, o.value); return api; },
      addCombo: function (o) { types[o.name] = 'combo'; props[o.name] = read(o.name, o.value); return api; },
      addText: function (o) { types[o.name] = 'text'; var v = read(o.name, o.value || ''); props[o.name] = String(v); return api; },
      addColor: function (o) { types[o.name] = 'color'; props[o.name] = parseVec(read(o.name, o.value)); return api; },
      addDirection: function (o) { types[o.name] = 'direction'; props[o.name] = parseVec(read(o.name, o.value)); return api; },
      addFile: function (o) { types[o.name] = 'file'; var v = read(o.name, o.value || ''); props[o.name] = String(v); return api; },
      addFont: function (o) { types[o.name] = 'font'; var v = read(o.name, o.value || ''); props[o.name] = String(v); return api; },
      finish: function () {
        if (globalThis.__WE_current_sp_meta) {
          globalThis.__WE_current_sp_meta.props = props;
          globalThis.__WE_current_sp_meta.types = types;
        }
        return props;
      }
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
    var seen = {};
    var uniq = [];
    for (var ni = 0; ni < names.length; ni++) {
      if (!Object.prototype.hasOwnProperty.call(seen, names[ni])) {
        seen[names[ni]] = true;
        uniq.push(names[ni]);
      }
    }
    return { body: body, names: uniq };
  }

  function snapshotLayers() {
    var snap = {};
    for (var i = 0; i < layerOrder.length; i++) {
      var id = layerOrder[i];
      var l = layers[id];
      snap[id] = {
        visible: !!l.visible,
        solid: !!l.solid,
        alpha: num(l.alpha, 1),
        volume: num(l.volume, 1),
        origin: toArr3(l.origin),
        scale: toArr3(l.scale),
        angles: toArr3(l.angles),
        color: toArr3(l.color),
        text: l.text === undefined || l.text === null ? '' : String(l.text),
        alignment: l.alignment === undefined || l.alignment === null ? '' : String(l.alignment),
        horizontalalign: l.horizontalalign === undefined || l.horizontalalign === null ? '' : String(l.horizontalalign),
        verticalalign: l.verticalalign === undefined || l.verticalalign === null ? '' : String(l.verticalalign),
        pointsize: num(l.pointsize, 32),
        font: l.font === undefined || l.font === null ? '' : String(l.font)
      };
    }
    return snap;
  }

  function snapshotEffects() {
    var snap = [];
    for (var i = 0; i < allEffects.length; i++) {
      snap.push(!!allEffects[i].visible);
    }
    return snap;
  }

  function snapshotAnimLayers() {
    var snap = [];
    for (var i = 0; i < allAnimLayers.length; i++) {
      var al = allAnimLayers[i];
      snap.push({
        visible: !!al.visible,
        blend: num(al.blend, 1),
        rate: num(al.__rate, 1)
      });
    }
    return snap;
  }

  function collectOutputs(preLayers, preEffects, preAnimLayers, forcedLayers, forcedEffects, forcedAnimLayers, errors) {
    var outputs = pendingSceneOps.slice();
    pendingSceneOps = [];
    for (var i = 0; i < layerOrder.length; i++) {
      var idStr = layerOrder[i];
      var l = layers[idStr];
      var prev = preLayers[idStr];
      var isNew = !prev && !!createdThisTick[idStr];
      if (!prev && !isNew) continue;
      var numId = l.id;
      var curVis = !!l.visible;
      if (isNew || curVis !== prev.visible || forcedLayers[idStr + ':visible']) {
        outputs.push({ kind: 'layer', id: numId, field: 'visible', value: curVis });
      }
      var curAlpha = num(l.alpha, 1);
      if (isNew || curAlpha !== prev.alpha || forcedLayers[idStr + ':alpha']) {
        outputs.push({ kind: 'layer', id: numId, field: 'alpha', value: curAlpha });
      }
      var vecFields = ['origin', 'scale', 'angles', 'color'];
      for (var fi = 0; fi < vecFields.length; fi++) {
        var f = vecFields[fi];
        var curVec = toArr3(l[f]);
        if (isNew || !vec3Eq(curVec, prev[f]) || forcedLayers[idStr + ':' + f]) {
          outputs.push({ kind: 'layer', id: numId, field: f, value: curVec });
        }
      }
      var strFields = ['text', 'alignment', 'horizontalalign', 'verticalalign', 'font'];
      for (var si = 0; si < strFields.length; si++) {
        var sf = strFields[si];
        var curStr = l[sf] === undefined || l[sf] === null ? '' : String(l[sf]);
        if (isNew || curStr !== prev[sf] || forcedLayers[idStr + ':' + sf]) {
          outputs.push({ kind: 'layer', id: numId, field: sf, value: curStr });
        }
      }
      var curPs = num(l.pointsize, 32);
      if (isNew || curPs !== prev.pointsize || forcedLayers[idStr + ':pointsize']) {
        outputs.push({ kind: 'layer', id: numId, field: 'pointsize', value: curPs });
      }
      var curSolid = !!l.solid;
      if (isNew || curSolid !== prev.solid) {
        outputs.push({ kind: 'layer_solid', id: numId, value: curSolid });
      }
      if (l.__isSound && prev) {
        var curVol = num(l.volume, 1);
        if (curVol !== prev.volume || forcedLayers[idStr + ':volume']) {
          outputs.push({ kind: 'layer', id: numId, field: 'volume', value: curVol });
        }
        if (l.__soundAction !== null) {
          outputs.push({
            kind: 'sound_ctrl',
            layerId: numId,
            value: { action: l.__soundAction }
          });
          l.__soundAction = null;
        }
      }
    }
    createdThisTick = {};

    for (var e = 0; e < allEffects.length; e++) {
      var eff = allEffects[e];
      var curEffVis = !!eff.visible;
      var eKey = eff.__layerId + ':' + eff.__effectIdx;
      if (curEffVis !== preEffects[e] || forcedEffects[eKey]) {
        outputs.push({
          kind: 'effect_visible',
          layerId: eff.__layerId,
          effectIdx: eff.__effectIdx,
          value: curEffVis
        });
      }
    }

    for (var ai = 0; ai < allAnimLayers.length; ai++) {
      var al = allAnimLayers[ai];
      var pa = preAnimLayers[ai];
      var aKey = al.__layerId + ':' + al.__animLayerIdx;
      var curVis = !!al.visible;
      if (curVis !== pa.visible || forcedAnimLayers[aKey + ':visible']) {
        outputs.push({
          kind: 'anim_layer_field',
          layerId: al.__layerId,
          animLayerIdx: al.__animLayerIdx,
          field: 'visible',
          value: curVis
        });
      }
      var curBlend = num(al.blend, 1);
      if (curBlend !== pa.blend || forcedAnimLayers[aKey + ':blend']) {
        outputs.push({
          kind: 'anim_layer_field',
          layerId: al.__layerId,
          animLayerIdx: al.__animLayerIdx,
          field: 'blend',
          value: curBlend
        });
      }
      var curRate = num(al.__rate, 1);
      if (curRate !== pa.rate || forcedAnimLayers[aKey + ':rate']) {
        outputs.push({
          kind: 'anim_layer_field',
          layerId: al.__layerId,
          animLayerIdx: al.__animLayerIdx,
          field: 'rate',
          value: curRate
        });
      }
    }

    for (var ka = 0; ka < allKeyAnims.length; ka++) {
      var a = allKeyAnims[ka];
      if (!a.__dirty || !a.__ctrlKind) continue;
      a.__dirty = false;
      var payload = {
        kind: a.__ctrlKind,
        value: {
          playing: !!a.__playing,
          frame: num(a.__frame, 0),
          rate: num(a.__rate, 1),
          mirrorReverse: !!a.__mirrorReverse
        }
      };
      var tgt = a.__ctrlTarget || {};
      for (var tk in tgt) {
        if (Object.prototype.hasOwnProperty.call(tgt, tk)) payload[tk] = tgt[tk];
      }
      outputs.push(payload);
    }

    for (var ta = 0; ta < allTexAnims.length; ta++) {
      var tx = allTexAnims[ta];
      if (!tx.__controlled) continue;
      var curF = Math.floor(num(tx.__frame, 0));
      if (tx.__dirty || (tx.__playing && tx.__rate !== 0 && curF !== tx.__lastEmittedFrame)) {
        tx.__dirty = false;
        tx.__lastEmittedFrame = curF;
        outputs.push({
          kind: 'tex_anim',
          layerId: tx.__layerId,
          value: {
            frame: curF,
            playing: !!tx.__playing,
            rate: num(tx.__rate, 1)
          }
        });
      }
    }

    for (var vt = 0; vt < allVideoTexs.length; vt++) {
      var vd = allVideoTexs[vt];
      if (!vd.__dirty) continue;
      vd.__dirty = false;
      var sk = vd.__pendingSeek;
      vd.__pendingSeek = null;
      outputs.push({
        kind: 'video_tex',
        layerId: vd.__layerId,
        value: {
          playing: !!vd.__playing,
          rate: num(vd.playbackRate, 1),
          seek: sk
        }
      });
    }

    for (var s = 0; s < scripts.length; s++) {
      var b = scripts[s];
      if (b.dead || !b.dirty) continue;
      if (b.kind === 'effect_const') {
        b.dirty = false;
        outputs.push({
          kind: 'effect_const',
          layerId: b.layerId,
          effectIdx: b.effectIdx,
          passIdx: b.passIdx,
          key: b.key,
          value: serializeConstValue(b.current)
        });
      } else if (b.kind === 'instance_override') {
        b.dirty = false;
        outputs.push({
          kind: 'instance_override',
          layerId: b.layerId,
          key: b.key,
          value: serializeConstValue(b.current)
        });
      }
    }

    var resp = {
      errors: errors,
      outputs: outputs,
      usesAudio: audioRegistered
    };
    if (storageDirty) {
      storageDirty = false;
      resp.storage = serializeStorageVal(storage, 0);
    }
    return JSON.stringify(resp);
  }

  function applyBindingValue(b, ret, forcedLayers, forcedEffects, forcedAnimLayers) {
    if (ret === undefined) return;
    if (b.kind === 'layer') {
      var norm = normalizeField(b.field, ret);
      b.layer[b.field] = norm;
      forcedLayers[String(b.id) + ':' + b.field] = true;
    } else if (b.kind === 'effect_visible') {
      b.effect.visible = !!ret;
      forcedEffects[b.layerId + ':' + b.effectIdx] = true;
    } else if (b.kind === 'effect_const' || b.kind === 'instance_override') {
      b.current = normalizeConstValue(ret, b.valueKind);
      b.dirty = true;
    } else if (b.kind === 'anim_layer') {
      if (b.field === 'visible') b.animLayer.visible = !!ret;
      else b.animLayer[b.field] = num(ret, b.initial);
      forcedAnimLayers[b.layerId + ':' + b.animLayerIdx + ':' + b.field] = true;
    }
  }

  function runTimers() {
    var limit = 64;
    var rt = globalThis.engine.runtime;
    while (timers.length && limit-- > 0) {
      var due = null;
      for (var t = 0; t < timers.length; t++) {
        if (timers[t].at <= rt) {
          due = timers[t];
          if (due.interval > 0) {
            due.at = rt + due.interval;
          } else {
            timers.splice(t, 1);
          }
          break;
        }
      }
      if (!due) break;
      if (due.cancelled) continue;
      globalThis.thisScene = scene;
      globalThis.thisLayer = due.layer;
      globalThis.thisObject = due.obj;
      currentBinding = due.binding || null;
      try { due.fn(); } catch (_err) {}
    }
    currentBinding = null;
  }

  var init = {
    addLayer: function (jsonText) {
      var d = JSON.parse(jsonText);
      var l = makeLayer(d.id, d.name);
      l.kind = d.kind || '';
      l.visible = d.visible !== false;
      l.solid = d.solid !== false;
      l.alpha = num(d.alpha, 1);
      l.origin = parseVec(d.origin);
      l.scale = parseVec(d.scale);
      l.angles = parseVec(d.angles);
      l.color = parseVec(d.color);
      l.size = new Vec2(d.size && d.size[0], d.size && d.size[1]);
      l.alignment = typeof d.align === 'string' && d.align ? d.align : 'center';
      l.horizontalalign = typeof d.horizontalalign === 'string' && d.horizontalalign ? d.horizontalalign : 'center';
      l.verticalalign = typeof d.verticalalign === 'string' && d.verticalalign ? d.verticalalign : 'center';
      l.pointsize = num(d.pointsize, 32);
      l.font = typeof d.font === 'string' ? d.font : '';
      l.image = typeof d.image === 'string' ? d.image : '';
      l.text = d.text || '';
      l._origOrigin = l.origin.copy();
      l._origScale = l.scale.copy();
      l._origAngles = l.angles.copy();
      l._origSize = l.size.copy();
      l._origColor = l.color.copy();
      l._origAlpha = l.alpha;
      l.__parentId = d.parent;
      if (d.texAnim) l.__tex = makeTexAnim(d.id, d.texAnim);
      if (d.videoTex) l.__video = makeVideoTex(d.id, d.videoTex);
      if (d.sound) {
        l.__isSound = true;
        l.volume = num(d.sound.volume, 1);
        l.__playing = !!d.sound.playing;
      }
      layers[String(d.id)] = l;
      layerOrder.push(String(d.id));
      return true;
    },
    addEffect: function (jsonText) {
      var d = JSON.parse(jsonText);
      var layer = layers[String(d.layerId)];
      if (!layer) return false;
      var eff = {
        id: d.id != null ? d.id : d.effectIdx,
        name: d.name || '',
        visible: d.visible !== false,
        color: new Vec3(1, 1, 1),
        __layer: layer,
        __layerId: d.layerId,
        __effectIdx: d.effectIdx,
        __propAnims: [],
        getLayer: function () { return layer; },
        getAnimation: function (ref) {
          if (ref === undefined && currentBinding && currentBinding.propAnim && currentBinding.effect === eff) {
            return currentBinding.propAnim;
          }
          if (ref !== undefined) return findNamedAnim(eff.__propAnims, ref) || null;
          return eff.__propAnims[0] || animStub();
        }
      };
      layer.__effects[String(d.effectIdx)] = eff;
      layer.__effectList.push(eff);
      allEffects.push(eff);
      return true;
    },
    addAnimLayer: function (jsonText) {
      var d = JSON.parse(jsonText);
      var layer = layers[String(d.layerId)];
      if (!layer) return false;
      var fc = Math.max(1, num(d.frameCount, 1));
      var al = makeKeyAnim({
        name: d.name || '',
        fps: num(d.fps, 30),
        frameCount: fc,
        maxFrame: Math.max(0, fc - 1),
        duration: num(d.duration, 0),
        mode: d.mode || 'loop',
        startPaused: false,
        rate: num(d.rate, 1),
        layer: layer,
        obj: null
      }, 'anim_layer_ctrl', { layerId: d.layerId, animLayerIdx: d.animLayerIdx });
      al.id = d.id != null ? d.id : d.animLayerIdx;
      al.visible = d.visible !== false;
      al.blend = num(d.blend, 1);
      al.__layer = layer;
      al.__layerId = d.layerId;
      al.__animLayerIdx = d.animLayerIdx;
      al.__obj = al;
      al.getLayer = function () { return layer; };
      layer.__animLayers[String(d.animLayerIdx)] = al;
      layer.__animLayerList.push(al);
      allAnimLayers.push(al);
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
      var kind = d.kind || 'layer';
      var layer = layers[String(kind === 'layer' ? d.id : d.layerId)] || null;
      var eff = (kind === 'effect_visible' || kind === 'effect_const') && layer
        ? (layer.__effects[String(d.effectIdx)] || null)
        : null;
      var al = kind === 'anim_layer' && layer
        ? (layer.__animLayers[String(d.animLayerIdx)] || null)
        : null;

      var propAnim = null;
      if (d.propAnim) {
        if (kind === 'layer' && layer) {
          propAnim = makeKeyAnim({
            name: d.propAnim.name || '',
            fps: d.propAnim.fps,
            length: d.propAnim.length,
            maxFrame: d.propAnim.length,
            mode: d.propAnim.mode,
            startPaused: !!d.propAnim.startPaused,
            layer: layer,
            obj: layer
          }, 'layer_anim_ctrl', { id: d.id, field: d.field });
          layer.__propAnims.push(propAnim);
        } else if (kind === 'effect_const' && eff) {
          propAnim = makeKeyAnim({
            name: d.propAnim.name || '',
            fps: d.propAnim.fps,
            length: d.propAnim.length,
            maxFrame: d.propAnim.length,
            mode: d.propAnim.mode,
            startPaused: !!d.propAnim.startPaused,
            layer: layer,
            obj: eff
          }, 'effect_const_anim_ctrl', {
            layerId: d.layerId,
            effectIdx: d.effectIdx,
            passIdx: d.passIdx,
            key: d.key || ''
          });
          eff.__propAnims.push(propAnim);
        }
      } else if (kind === 'anim_layer' && al) {
        propAnim = al;
      }

      var entry = {
        kind: kind,
        id: d.id || 0,
        field: d.field || '',
        layerId: d.layerId || 0,
        effectIdx: d.effectIdx || 0,
        passIdx: d.passIdx || 0,
        animLayerIdx: d.animLayerIdx || 0,
        key: d.key || '',
        mod: null,
        layer: layer,
        effect: eff,
        animLayer: al,
        propAnim: propAnim,
        scriptPropsObj: null,
        scriptPropTypes: {},
        dead: false,
        error: null,
        dirty: false
      };

      var prepared = prepare(d.source);
      var mod = null;
      var compileErr = null;
      var spMeta = { props: null, types: {} };
      globalThis.__WE_binding_props = d.scriptProperties || {};
      globalThis.__WE_current_sp_meta = spMeta;
      globalThis.thisScene = scene;
      globalThis.thisLayer = layer;
      globalThis.thisObject = al || eff || layer;
      currentBinding = entry;
      try {
        var factory = (0, eval)('(function(){\n' + prepared.body + '\nreturn {' +
          prepared.names.map(function (n) {
            return n + ':(typeof ' + n + '===\'undefined\'?undefined:' + n + ')';
          }).join(',') + '};\n})');
        mod = factory();
      } catch (e) {
        compileErr = errText(e);
      } finally {
        globalThis.__WE_binding_props = {};
        globalThis.__WE_current_sp_meta = null;
        currentBinding = null;
      }

      entry.mod = mod;
      entry.scriptPropsObj = spMeta.props;
      entry.scriptPropTypes = spMeta.types;
      entry.dead = !!compileErr;
      entry.error = compileErr;
      var hasCursor = !!(mod && (
        typeof mod.cursorEnter === 'function' ||
        typeof mod.cursorLeave === 'function' ||
        typeof mod.cursorDown === 'function' ||
        typeof mod.cursorUp === 'function' ||
        typeof mod.cursorClick === 'function' ||
        typeof mod.cursorMove === 'function'
      ));
      entry.hasCursor = hasCursor;
      var hasMedia = !!(mod && (
        typeof mod.mediaStatusChanged === 'function' ||
        typeof mod.mediaPlaybackChanged === 'function' ||
        typeof mod.mediaPropertiesChanged === 'function' ||
        typeof mod.mediaTimelineChanged === 'function' ||
        typeof mod.mediaThumbnailChanged === 'function'
      ));
      entry.hasMedia = hasMedia;

      if (kind === 'layer') {
        entry.initial = normalizeField(d.field, d.initial);
      } else if (kind === 'effect_visible') {
        entry.initial = !!d.initial;
      } else if (kind === 'anim_layer') {
        entry.initial = d.field === 'visible' ? !!d.initial : num(d.initial, 1);
      } else {
        var ci = parseConstInitial(d.initial);
        entry.initial = ci.value;
        entry.current = copyConstValue(ci.value);
        entry.valueKind = ci.kind;
      }
      scripts.push(entry);
      return JSON.stringify({
        ok: !compileErr,
        error: compileErr,
        usesAudio: audioRegistered,
        hasCursor: hasCursor,
        hasMedia: hasMedia
      });
    },
    finalizeInit: function () {
      var preLayers = snapshotLayers();
      var preEffects = snapshotEffects();
      var preAnimLayers = snapshotAnimLayers();
      var forcedLayers = {};
      var forcedEffects = {};
      var forcedAnimLayers = {};
      var errors = [];

      for (var k = 0; k < scripts.length; k++) {
        var b = scripts[k];
        if (b.dead) continue;
        if ((b.kind === 'layer' || b.kind === 'instance_override') && !b.layer) {
          b.dead = true;
          errors.push({ idx: k, kind: 'init', message: 'layer missing' });
          continue;
        }
        if ((b.kind === 'effect_visible' || b.kind === 'effect_const') && !b.effect) {
          b.dead = true;
          errors.push({ idx: k, kind: 'init', message: 'effect missing' });
          continue;
        }
        if (b.kind === 'anim_layer' && !b.animLayer) {
          b.dead = true;
          errors.push({ idx: k, kind: 'init', message: 'animLayer missing' });
          continue;
        }
        if (b.mod && typeof b.mod.init === 'function') {
          try {
            globalThis.thisScene = scene;
            globalThis.thisLayer = b.layer;
            globalThis.thisObject = b.animLayer || b.effect || b.layer;
            currentBinding = b;
            var arg = copyConstValue(b.initial);
            var ret = b.mod.init(arg);
            if ((b.kind === 'effect_const' || b.kind === 'instance_override') && ret === undefined &&
              (arg instanceof Vec2 || arg instanceof Vec3 || arg instanceof Vec4) &&
              !arg.equals(b.initial)) {
              ret = arg;
            }
            applyBindingValue(b, ret, forcedLayers, forcedEffects, forcedAnimLayers);
          } catch (e) {
            b.dead = true;
            b.error = errText(e);
            errors.push({ idx: k, kind: 'init', message: b.error });
          } finally {
            currentBinding = null;
          }
        }
      }

      for (var k = 0; k < scripts.length; k++) {
        var b = scripts[k];
        if (b.dead) continue;
        if (b.mod && typeof b.mod.applyUserProperties === 'function') {
          try {
            globalThis.thisScene = scene;
            globalThis.thisLayer = b.layer;
            globalThis.thisObject = b.animLayer || b.effect || b.layer;
            currentBinding = b;
            b.mod.applyUserProperties(userProperties);
          } catch (e) {
            b.dead = true;
            b.error = errText(e);
            errors.push({ idx: k, kind: 'applyUserProperties', message: b.error });
          } finally {
            currentBinding = null;
          }
        }
      }

      return collectOutputs(preLayers, preEffects, preAnimLayers, forcedLayers, forcedEffects, forcedAnimLayers, errors);
    },
    applyUserProperties: function (jsonText) {
      var d = JSON.parse(jsonText);
      var changed = d.changed || {};
      var bindingProps = d.bindingProps || {};
      for (var key in changed) {
        if (Object.prototype.hasOwnProperty.call(changed, key)) {
          userProperties[key] = changed[key];
        }
      }

      var preLayers = snapshotLayers();
      var preEffects = snapshotEffects();
      var preAnimLayers = snapshotAnimLayers();
      var forcedLayers = {};
      var forcedEffects = {};
      var forcedAnimLayers = {};
      var errors = [];

      for (var k = 0; k < scripts.length; k++) {
        var b = scripts[k];
        if (b.dead) continue;
        var resolvedOverride = bindingProps[String(k)];
        if (b.scriptPropsObj) {
          var types = b.scriptPropTypes || {};
          if (resolvedOverride) {
            for (var pname in resolvedOverride) {
              if (Object.prototype.hasOwnProperty.call(resolvedOverride, pname)) {
                b.scriptPropsObj[pname] = coerceScriptProp(types[pname], resolvedOverride[pname]);
              }
            }
          }
          for (var pname in b.scriptPropsObj) {
            if ((!resolvedOverride || !Object.prototype.hasOwnProperty.call(resolvedOverride, pname)) &&
                Object.prototype.hasOwnProperty.call(changed, pname)) {
              b.scriptPropsObj[pname] = coerceScriptProp(types[pname], changed[pname]);
            }
          }
        }
        if (b.mod && typeof b.mod.applyUserProperties === 'function') {
          try {
            globalThis.thisScene = scene;
            globalThis.thisLayer = b.layer;
            globalThis.thisObject = b.animLayer || b.effect || b.layer;
            currentBinding = b;
            b.mod.applyUserProperties(changed);
          } catch (e) {
            b.dead = true;
            b.error = errText(e);
            errors.push({ idx: k, kind: 'applyUserProperties', message: b.error });
          } finally {
            currentBinding = null;
          }
        }
      }

      return collectOutputs(preLayers, preEffects, preAnimLayers, forcedLayers, forcedEffects, forcedAnimLayers, errors);
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
      globalThis.input.cursorScreenPosition = s.mouseScreen
        ? new Vec2(s.mouseScreen[0], s.mouseScreen[1])
        : new Vec2(e.mousePosition.x, e.mousePosition.y);
      globalThis.input.cursorLeftDown = !!s.mouseDown;

      if (s.spectrum) {
        updateAudioBuffers(s.spectrum);
      }

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
        if (d.videoTime && l.__video && l.__video.__real) {
          l.__video.currentTime = num(d.videoTime[0], l.__video.currentTime);
          if (num(d.videoTime[1], 0) > 0) l.__video.duration = num(d.videoTime[1], l.__video.duration);
        }
        if (d.sound && l.__isSound) {
          l.volume = num(d.sound[0], l.volume);
          l.__playing = !!d.sound[1];
        }
      }

      var preLayers = snapshotLayers();
      var preEffects = snapshotEffects();
      var preAnimLayers = snapshotAnimLayers();
      var forcedLayers = {};
      var forcedEffects = {};
      var forcedAnimLayers = {};
      var errors = [];

      var dt = num(s.frametime, 0);
      for (var ka = 0; ka < allKeyAnims.length; ka++) advanceKeyAnim(allKeyAnims[ka], dt);
      for (var ta = 0; ta < allTexAnims.length; ta++) advanceTexAnim(allTexAnims[ta], dt, e.runtime);

      runTimers();

      if (Array.isArray(s.cursorEvents) && s.cursorEvents.length) {
        for (var ci = 0; ci < s.cursorEvents.length; ci++) {
          var ev = s.cursorEvents[ci];
          var targetLayer = layers[String(ev.layerId)];
          if (!targetLayer) continue;
          var eventObj = {
            worldPosition: new Vec3(ev.worldPos[0], ev.worldPos[1], 0),
            screenPosition: new Vec2(ev.screenPos[0], ev.screenPos[1]),
            rawPosition: new Vec2(ev.screenPos[0], ev.screenPos[1]),
            localPosition: new Vec3(ev.localPos[0], ev.localPos[1], 0),
            layer: targetLayer
          };
          for (var k = 0; k < scripts.length; k++) {
            var b = scripts[k];
            if (b.dead || !b.hasCursor || b.layer !== targetLayer || !b.mod) continue;
            var fn = b.mod[ev.kind];
            if (typeof fn !== 'function') continue;
            try {
              globalThis.thisScene = scene;
              globalThis.thisLayer = b.layer;
              globalThis.thisObject = b.animLayer || b.effect || b.layer;
              currentBinding = b;
              var ret = fn(eventObj);
              applyBindingValue(b, ret, forcedLayers, forcedEffects, forcedAnimLayers);
            } catch (err) {
              b.dead = true;
              b.error = errText(err);
              errors.push({ idx: k, kind: ev.kind, message: b.error });
            } finally {
              currentBinding = null;
            }
          }
        }
      }

      if (Array.isArray(s.mediaEvents) && s.mediaEvents.length) {
        for (var mi = 0; mi < s.mediaEvents.length; mi++) {
          var ev = s.mediaEvents[mi];
          var eventObj;
          if (ev.kind === 'mediaStatusChanged') {
            eventObj = { enabled: !!ev.enabled };
          } else if (ev.kind === 'mediaPlaybackChanged') {
            eventObj = { state: num(ev.state, 0) };
          } else if (ev.kind === 'mediaPropertiesChanged') {
            eventObj = {
              title: ev.title || '',
              artist: ev.artist || '',
              albumTitle: ev.albumTitle || '',
              albumArtist: ev.albumArtist || '',
              subTitle: ev.subTitle || '',
              genres: ev.genres || '',
              contentType: ev.contentType || 'music'
            };
          } else if (ev.kind === 'mediaTimelineChanged') {
            eventObj = {
              position: num(ev.position, 0),
              duration: num(ev.duration, 0)
            };
          } else if (ev.kind === 'mediaThumbnailChanged') {
            eventObj = {
              hasThumbnail: !!ev.hasThumbnail,
              primaryColor: parseVec(ev.primaryColor),
              secondaryColor: parseVec(ev.secondaryColor),
              tertiaryColor: parseVec(ev.tertiaryColor),
              textColor: parseVec(ev.textColor),
              highContrastColor: parseVec(ev.highContrastColor)
            };
          } else {
            continue;
          }
          for (var k = 0; k < scripts.length; k++) {
            var b = scripts[k];
            if (b.dead || !b.hasMedia || !b.mod) continue;
            var fn = b.mod[ev.kind];
            if (typeof fn !== 'function') continue;
            try {
              globalThis.thisScene = scene;
              globalThis.thisLayer = b.layer;
              globalThis.thisObject = b.animLayer || b.effect || b.layer;
              currentBinding = b;
              var ret = fn(eventObj);
              applyBindingValue(b, ret, forcedLayers, forcedEffects, forcedAnimLayers);
            } catch (err) {
              b.dead = true;
              b.error = errText(err);
              errors.push({ idx: k, kind: ev.kind, message: b.error });
            } finally {
              currentBinding = null;
            }
          }
        }
      }

      for (var k = 0; k < scripts.length; k++) {
        var b = scripts[k];
        if (b.dead) continue;
        if ((b.kind === 'layer' || b.kind === 'instance_override') && !b.layer) {
          b.dead = true;
          errors.push({ idx: k, kind: 'runtime', message: 'layer missing' });
          continue;
        }
        if ((b.kind === 'effect_visible' || b.kind === 'effect_const') && !b.effect) {
          b.dead = true;
          errors.push({ idx: k, kind: 'runtime', message: 'effect missing' });
          continue;
        }
        if (b.kind === 'anim_layer' && !b.animLayer) {
          b.dead = true;
          errors.push({ idx: k, kind: 'runtime', message: 'animLayer missing' });
          continue;
        }
        if (!b.mod || typeof b.mod.update !== 'function') continue;

        try {
          globalThis.thisScene = scene;
          globalThis.thisLayer = b.layer;
          globalThis.thisObject = b.animLayer || b.effect || b.layer;
          currentBinding = b;

          if (b.kind === 'layer') {
            var layer = b.layer;
            var value;
            if (b.field === 'text') value = layer.text;
            else if (b.field === 'visible') value = layer.visible;
            else if (b.field === 'alpha') value = layer.alpha;
            else if (b.field === 'volume') value = layer.volume;
            else value = layer[b.field] ? layer[b.field].copy() : null;

            var res = b.mod.update(value);
            if (b.field === 'text') {
              if (res !== undefined && res !== null) layer.text = String(res);
            } else if (b.field === 'visible') {
              if (res !== undefined) layer.visible = !!res;
            } else if (b.field === 'alpha') {
              if (res !== undefined) layer.alpha = num(res, layer.alpha);
            } else if (b.field === 'volume') {
              if (res !== undefined) layer.volume = num(res, layer.volume);
            } else if (res !== undefined && res !== null) {
              layer[b.field] = parseVec(res);
            } else if (value instanceof Vec3 && preLayers[String(b.id)] &&
              !vec3Eq(toArr3(value), preLayers[String(b.id)][b.field])) {
              layer[b.field] = value;
            }
            forcedLayers[String(b.id) + ':' + b.field] = true;
          } else if (b.kind === 'effect_visible') {
            var res = b.mod.update(!!b.effect.visible);
            if (res !== undefined) b.effect.visible = !!res;
            forcedEffects[b.layerId + ':' + b.effectIdx] = true;
          } else if (b.kind === 'anim_layer') {
            var curVal = b.field === 'visible' ? !!b.animLayer.visible : num(b.animLayer[b.field], 1);
            var res = b.mod.update(curVal);
            if (res !== undefined) {
              if (b.field === 'visible') b.animLayer.visible = !!res;
              else b.animLayer[b.field] = num(res, curVal);
            }
            forcedAnimLayers[b.layerId + ':' + b.animLayerIdx + ':' + b.field] = true;
          } else if (b.kind === 'effect_const' || b.kind === 'instance_override') {
            var arg = copyConstValue(b.current);
            var res = b.mod.update(arg);
            if (res !== undefined && res !== null) {
              b.current = normalizeConstValue(res, b.valueKind);
              b.dirty = true;
            } else if ((arg instanceof Vec2 || arg instanceof Vec3 || arg instanceof Vec4) &&
              !arg.equals(b.current)) {
              b.current = arg;
              b.dirty = true;
            }
          }
        } catch (err) {
          b.dead = true;
          b.error = errText(err);
          errors.push({ idx: k, kind: 'runtime', message: b.error });
        } finally {
          currentBinding = null;
        }
      }
      return collectOutputs(preLayers, preEffects, preAnimLayers, forcedLayers, forcedEffects, forcedAnimLayers, errors);
    }
  };

  globalThis.__WE_init = function (jsonText) {
    var d = JSON.parse(jsonText);
    userProperties = d.userProperties || {};
    globalThis.engine.userProperties = userProperties;
    if (d.storage && typeof d.storage === 'object') {
      for (var sk in d.storage) {
        if (Object.prototype.hasOwnProperty.call(d.storage, sk)) {
          storage[sk] = deserializeStorageVal(d.storage[sk], 0);
        }
      }
      storageDirty = false;
    }
    return true;
  };
  globalThis.__WE_addLayer = init.addLayer;
  globalThis.__WE_addEffect = init.addEffect;
  globalThis.__WE_addAnimLayer = init.addAnimLayer;
  globalThis.__WE_addScript = init.addScript;
  globalThis.__WE_finalizeInit = init.finalizeInit;
  globalThis.__WE_applyUserProperties = init.applyUserProperties;
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
            solid: true,
            alpha: 1.0,
            origin: [0.0, 0.0, 0.0],
            scale: [1.0, 1.0, 1.0],
            angles: [0.0, 0.0, 0.0],
            color: [1.0, 1.0, 1.0],
            size: [100.0, 100.0],
            align: "center".into(),
            horizontal_align: "center".into(),
            vertical_align: "center".into(),
            point_size: 32.0,
            font: String::new(),
            image: String::new(),
            text: String::new(),
            parent: None,
            tex_anim: None,
            video_tex: None,
            sound: None,
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
            video_time: None,
            sound: None,
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
            None,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(1)],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].target,
            ScriptTarget::Layer {
                id: 1,
                field: ScriptField::Scale
            }
        );
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
            None,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(7)],
        );
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
            None,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(3)],
        );
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
            None,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(4)],
        );
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
            None,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(5), frame(6)],
        );
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
            None,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(11)],
        );
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
            None,
        );
        let layers = [frame(9)];
        let _ = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &layers,
        );
        let out = host.tick(
            0.0,
            0.016,
            0.0,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &layers,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn effect_constant_script_scalar_vec3_shared_and_wecolor() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(10, "bars"));
        host.add_effect(&EffectSeed {
            layer_id: 10,
            effect_idx: 0,
            id: Some(100),
            name: "gradient".into(),
            visible: true,
        });
        host.add_property_script(
            10,
            ScriptField::Alpha,
            "import * as WEColor from 'WEColor';\nexport function update() {\n  shared.primaryColor = WEColor.hsv2rgb({ x: 0.0, y: 1.0, z: 1.0 });\n  return 1.0;\n}",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        host.add_effect_constant_script(
            10,
            0,
            0,
            "Color 1",
            "export function update() { return shared.primaryColor; }",
            json!("0 0 1"),
            &serde_json::Map::new(),
            None,
        );
        host.add_effect_constant_script(
            10,
            0,
            0,
            "Opacity",
            "export function update(v) { return v * 0.5; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let _ = host.finalize_init();
        let out = host.tick(
            0.1,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(10)],
        );
        let col = out
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::EffectConstant {
                        layer_id: 10,
                        effect_idx: 0,
                        pass_idx: 0,
                        key: "color 1".into(),
                    }
            })
            .expect("color 1 output");
        let arr = col.value.as_array().expect("vec3");
        assert!((arr[0].as_f64().unwrap() - 1.0).abs() < 1e-5);
        assert!(arr[1].as_f64().unwrap().abs() < 1e-5);
        assert!(arr[2].as_f64().unwrap().abs() < 1e-5);

        let op = out
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::EffectConstant {
                        layer_id: 10,
                        effect_idx: 0,
                        pass_idx: 0,
                        key: "opacity".into(),
                    }
            })
            .expect("opacity output");
        assert!((op.value.as_f64().unwrap() - 0.5).abs() < 1e-5);
    }

    #[test]
    fn effect_visible_script_and_set_timeout_cancel_and_interval() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(20, "cover"));
        host.add_effect(&EffectSeed {
            layer_id: 20,
            effect_idx: 0,
            id: Some(200),
            name: "Tint".into(),
            visible: true,
        });
        host.add_effect_visible_script(
            20,
            0,
            "let ticks = 0;\nlet iv;\nexport function init() {\n  const cancel = engine.setTimeout(() => { thisObject.visible = false; }, 100);\n  cancel();\n  engine.setTimeout(() => { thisLayer.getEffect('Tint').visible = false; }, 500);\n  iv = engine.setInterval(() => {\n    ticks++;\n    thisLayer.alpha = ticks / 10;\n    if (ticks >= 2) engine.clearInterval(iv);\n  }, 150);\n}",
            true,
            &serde_json::Map::new(),
        );
        let _ = host.finalize_init();

        let frames = [frame(20)];
        let out1 = host.tick(
            0.2,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &frames,
        );
        // Cancelled timeout did NOT fire; interval fired once (alpha = 0.1)
        assert!(out1.iter().all(|o| !matches!(
            o.target,
            ScriptTarget::EffectVisible { .. }
        )));
        let a1 = out1
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 20, field: ScriptField::Alpha })
            .expect("alpha from interval 1");
        assert!((a1.value.as_f64().unwrap() - 0.1).abs() < 1e-5);

        // At 0.4s: interval fires a 2nd time and clears itself; timeout not yet due.
        let mut f20 = frame(20);
        f20.alpha = 0.1;
        let out2 = host.tick(
            0.4,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f20],
        );
        let a2 = out2
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 20, field: ScriptField::Alpha })
            .expect("alpha from interval 2");
        assert!((a2.value.as_f64().unwrap() - 0.2).abs() < 1e-5);

        // At 0.6s: 500ms timeout fires and hides effect; interval stays cleared.
        let mut f20_3 = frame(20);
        f20_3.alpha = 0.2;
        let out3 = host.tick(
            0.6,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f20_3],
        );
        let ev = out3
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::EffectVisible {
                        layer_id: 20,
                        effect_idx: 0,
                    }
            })
            .expect("effect_visible from timeout");
        assert_eq!(ev.value, json!(false));
        assert!(out3.iter().all(|o| o.target != ScriptTarget::Layer { id: 20, field: ScriptField::Alpha }));
    }

    #[test]
    fn audio_buffers_populated_from_spectrum() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(30, "vu"));
        host.add_property_script(
            30,
            ScriptField::Scale,
            "const b16 = engine.registerAudioBuffers(engine.AUDIO_RESOLUTION_16);\nconst b32 = engine.registerAudioBuffers(engine.AUDIO_RESOLUTION_32);\nconst b64 = engine.registerAudioBuffers(engine.AUDIO_RESOLUTION_64);\nexport function update() { return new Vec3(b16.average[0], b32.left[0], b64.right[63]); }",
            json!("1 1 1"),
            &serde_json::Map::new(),
            None,
        );
        let _ = host.finalize_init();
        assert!(host.uses_audio());

        let mut spec = [0.0f32; 64];
        spec[0] = 0.2;
        spec[1] = 0.6;
        spec[2] = 0.8;
        spec[3] = 0.4;
        spec[63] = 0.9;
        let out = host.tick(
            0.1,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            Some(&spec),
            &[frame(30)],
        );
        assert_eq!(out.len(), 1);
        let arr = out[0].value.as_array().unwrap();
        assert!((arr[0].as_f64().unwrap() - 0.5).abs() < 1e-5);
        assert!((arr[1].as_f64().unwrap() - 0.4).abs() < 1e-5);
        assert!((arr[2].as_f64().unwrap() - 0.9).abs() < 1e-5);
    }

    #[test]
    fn side_effect_layer_mutation_and_init_return() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(40, "controller"));
        host.add_layer(&seed(41, "target"));
        host.add_property_script(
            40,
            ScriptField::Alpha,
            "export function init(v) { return 0.4; }\nexport function update(v) { thisScene.getLayer('target').visible = false; return v; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        assert!(init_out.iter().any(|o| {
            o.target
                == ScriptTarget::Layer {
                    id: 40,
                    field: ScriptField::Alpha,
                }
                && (o.value.as_f64().unwrap() - 0.4).abs() < 1e-5
        }));

        let out = host.tick(
            0.1,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(40), frame(41)],
        );
        assert!(out.iter().any(|o| {
            o.target
                == ScriptTarget::Layer {
                    id: 41,
                    field: ScriptField::Visible,
                }
                && o.value == json!(false)
        }));
    }

    #[test]
    fn apply_user_properties_updates_in_place_preserving_shared() {
        let mut init_props = serde_json::Map::new();
        init_props.insert("speed".into(), json!(1.0));
        let mut host = SceneScripts::new(&init_props).expect("host");
        host.add_layer(&seed(50, "rain"));
        let mut sp = serde_json::Map::new();
        sp.insert("speed".into(), json!(1.0));
        let bidx = host.add_property_script(
            50,
            ScriptField::Alpha,
            "let calls = 0;\nexport var scriptProperties = createScriptProperties().addSlider({ name: 'speed', value: 1 }).finish();\nexport function applyUserProperties(changed) { calls++; shared.calls = calls; }\nexport function update() { return scriptProperties.speed * 0.1 + shared.calls; }",
            json!(1.0),
            &sp,
            None,
        );
        let _ = host.finalize_init();

        let mut changed = serde_json::Map::new();
        changed.insert("speed".into(), json!(3.0));
        let mut updated_sp = serde_json::Map::new();
        updated_sp.insert("speed".into(), json!(3.0));
        let _ = host.apply_user_properties(&changed, &[(bidx, updated_sp)]);

        let out = host.tick(
            0.1,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(50)],
        );
        // calls = 2 (1 at finalizeInit, 1 at applyUserProperties), speed = 3.0 -> 2.3
        assert!((out[0].value.as_f64().unwrap() - 2.3).abs() < 1e-5);
    }

    #[test]
    fn event_only_effect_constant_script_does_not_emit() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(60, "album"));
        host.add_effect(&EffectSeed {
            layer_id: 60,
            effect_idx: 0,
            id: Some(600),
            name: "blend".into(),
            visible: true,
        });
        host.add_effect_constant_script(
            60,
            0,
            0,
            "multiply",
            "export function mediaThumbnailChanged(e) { thisObject.getAnimation().play(); }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        assert!(init_out.is_empty());
        let tick_out = host.tick(
            0.1,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(60)],
        );
        assert!(tick_out.is_empty());
    }

    #[test]
    fn timer_restores_context_top_level_this_layer_and_size_is_value_copied() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        let mut s70 = seed(70, "layerA");
        s70.size = [200.0, 100.0];
        host.add_layer(&s70);
        host.add_layer(&seed(71, "layerB"));
        host.add_effect(&EffectSeed {
            layer_id: 70,
            effect_idx: 0,
            id: Some(700),
            name: "GlowA".into(),
            visible: true,
        });
        host.add_effect_visible_script(
            70,
            0,
            "const topNames = thisLayer.name + ':' + thisObject.name;\nexport function init() {\n  engine.setTimeout(() => {\n    shared.timerOk = (thisLayer && thisLayer.id === 70 && thisObject && thisObject.name === 'GlowA');\n    thisObject.visible = false;\n  }, 100);\n}\nexport function update(v) {\n  shared.topOk = (topNames === 'layerA:GlowA');\n  return v;\n}",
            true,
            &serde_json::Map::new(),
        );
        host.add_property_script(
            70,
            ScriptField::Origin,
            "const topLayerSelf = (thisLayer === thisObject && thisLayer.name === 'layerA');\nexport function update() {\n  let s = thisLayer.size;\n  s.x *= 0.5;\n  return new Vec3(s.x, thisLayer.size.x, (topLayerSelf && shared.topOk && shared.timerOk) ? 1 : 0);\n}",
            json!("0 0 0"),
            &serde_json::Map::new(),
            None,
        );
        host.add_property_script(
            71,
            ScriptField::Alpha,
            "export function update() { return 0.75; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );

        let _ = host.finalize_init();
        let frames = [frame(70), frame(71)];
        let out1 = host.tick(
            0.05,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &frames,
        );
        let o1 = out1
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 70, field: ScriptField::Origin })
            .expect("origin on frame 1");
        let arr1 = o1.value.as_array().expect("origin vec3");
        assert!((arr1[0].as_f64().unwrap() - 100.0).abs() < 1e-5);
        assert!((arr1[1].as_f64().unwrap() - 200.0).abs() < 1e-5);

        let out2 = host.tick(
            0.20,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &frames,
        );
        let ev = out2
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::EffectVisible {
                        layer_id: 70,
                        effect_idx: 0,
                    }
            })
            .expect("effect_visible on frame 2");
        assert_eq!(ev.value, json!(false));

        let o2 = out2
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 70, field: ScriptField::Origin })
            .expect("origin on frame 2");
        let arr2 = o2.value.as_array().expect("origin vec3 on frame 2");
        assert!((arr2[0].as_f64().unwrap() - 100.0).abs() < 1e-5);
        assert!((arr2[1].as_f64().unwrap() - 200.0).abs() < 1e-5);
        assert!((arr2[2].as_f64().unwrap() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn property_get_animation_startpaused_play_and_single_replay() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(80, "vinyl"));
        host.add_effect(&EffectSeed {
            layer_id: 80,
            effect_idx: 0,
            id: Some(800),
            name: "blend".into(),
            visible: true,
        });
        let pa_layer = PropAnimSeed {
            name: "Fade".into(),
            fps: 10.0,
            length: 10.0,
            mode: "single",
            start_paused: true,
        };
        let pa_eff = PropAnimSeed {
            name: "Spin".into(),
            fps: 10.0,
            length: 10.0,
            mode: "single",
            start_paused: true,
        };
        host.add_property_script(
            80,
            ScriptField::Alpha,
            "let ended = 0;\nlet ani;\nexport function init() {\n  ani = thisObject.getAnimation();\n  ani.addEndedCallback(() => { ended++; ani.stop(); ani.play(); });\n  engine.setTimeout(() => { ani.play(); }, 100);\n}\nexport function update(v) {\n  shared.alphaEnded = ended;\n  return v;\n}",
            json!(1.0),
            &serde_json::Map::new(),
            Some(&pa_layer),
        );
        host.add_effect_constant_script(
            80,
            0,
            0,
            "multiply",
            "let triggered = false;\nexport function init() {\n  let ea = thisObject.getAnimation();\n  engine.setTimeout(() => { if (!triggered) { triggered = true; ea.play(); } }, 50);\n}",
            json!(1.0),
            &serde_json::Map::new(),
            Some(&pa_eff),
        );
        let init_out = host.finalize_init();
        assert!(init_out.is_empty());

        // t=0.10: both timers fire -> play() emitted for layer & effect constant.
        let out1 = host.tick(
            0.10,
            0.10,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(80)],
        );
        assert!(out1.iter().any(|o| {
            o.target == ScriptTarget::LayerAnimControl { id: 80, field: ScriptField::Alpha }
                && o.value.get("playing") == Some(&json!(true))
        }));
        assert!(out1.iter().any(|o| {
            o.target
                == ScriptTarget::EffectConstAnimControl {
                    layer_id: 80,
                    effect_idx: 0,
                    pass_idx: 0,
                    key: "multiply".into(),
                }
                && o.value.get("playing") == Some(&json!(true))
        }));

        // t=1.20 (dt=1.10 -> 11 frames): single animation completes, ended callback replays from 0!
        let out2 = host.tick(
            1.20,
            1.10,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(80)],
        );
        let replay = out2
            .iter()
            .find(|o| o.target == ScriptTarget::LayerAnimControl { id: 80, field: ScriptField::Alpha })
            .expect("replay control emitted from endedCallback");
        assert_eq!(replay.value.get("playing"), Some(&json!(true)));
        assert!((replay.value.get("frame").unwrap().as_f64().unwrap() - 0.0).abs() < 1e-4);
    }

    #[test]
    fn texture_animation_set_frame_stop_rate_and_get_frame() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        let mut s = seed(85, "outline");
        s.tex_anim = Some(TexAnimSeed {
            frames: 32,
            duration: 6.4,
        });
        host.add_layer(&s);
        host.add_property_script(
            85,
            ScriptField::Alpha,
            "let anim;\nexport function init() {\n  anim = thisLayer.getTextureAnimation();\n  anim.stop();\n  anim.setFrame(3);\n  engine.setTimeout(() => { anim.rate = 10; anim.play(); }, 100);\n}\nexport function update(v) {\n  if (anim.getFrame() >= 8) anim.stop();\n  return v;\n}",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        let tex0 = init_out
            .iter()
            .find(|o| o.target == ScriptTarget::TextureAnimation { layer_id: 85 })
            .expect("initial tex anim state");
        assert_eq!(tex0.value.get("frame"), Some(&json!(3)));
        assert_eq!(tex0.value.get("playing"), Some(&json!(false)));

        // t=0.10: timer calls play() with rate=10 (50 fps) and advances cleanly.
        let out1 = host.tick(
            0.10,
            0.05,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(85)],
        );
        let tex1 = out1
            .iter()
            .find(|o| o.target == ScriptTarget::TextureAnimation { layer_id: 85 })
            .expect("playing tex anim");
        assert_eq!(tex1.value.get("playing"), Some(&json!(true)));
        assert!((tex1.value.get("rate").unwrap().as_f64().unwrap() - 10.0).abs() < 1e-4);

        // t=0.25 (dt=0.15s * 50fps = +7.5 frames -> frame >= 8 -> stopped at frame 0!).
        let out2 = host.tick(
            0.25,
            0.15,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(85)],
        );
        let tex2 = out2
            .iter()
            .find(|o| o.target == ScriptTarget::TextureAnimation { layer_id: 85 })
            .expect("stopped tex anim");
        assert_eq!(tex2.value.get("playing"), Some(&json!(false)));
        assert_eq!(tex2.value.get("frame"), Some(&json!(0)));
    }

    #[test]
    fn puppet_animation_layer_scripts_phase_offset_and_blend() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(90, "wing"));
        host.add_anim_layer(&AnimLayerSeed {
            layer_id: 90,
            anim_layer_idx: 0,
            id: Some(901),
            name: "Flap".into(),
            visible: true,
            blend: 1.0,
            rate: 1.0,
            frame_count: 100,
            fps: 30.0,
            duration: 99.0 / 30.0,
            mode: "loop",
        });
        host.add_anim_layer_script(
            90,
            0,
            AnimLayerField::Visible,
            "export var scriptProperties = createScriptProperties().addSlider({ name: 'percentage', value: 0.5 }).finish();\nexport function init(v) {\n  const ani = 'addEndedCallback' in thisObject ? thisObject : thisObject.getAnimation();\n  ani.play();\n  ani.setFrame(ani.frameCount * scriptProperties.percentage);\n  return v;\n}",
            json!(true),
            &serde_json::Map::new(),
            None,
        );
        host.add_anim_layer_script(
            90,
            0,
            AnimLayerField::Blend,
            "export function update() { return 0.42; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        let ctrl = init_out
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::PuppetAnimLayerControl {
                        layer_id: 90,
                        anim_layer_idx: 0,
                    }
            })
            .expect("puppet phase offset control");
        assert_eq!(ctrl.value.get("playing"), Some(&json!(true)));
        assert!((ctrl.value.get("frame").unwrap().as_f64().unwrap() - 50.0).abs() < 1e-4);

        let tick_out = host.tick(
            0.05,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(90)],
        );
        let blend = tick_out
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::PuppetAnimLayerField {
                        layer_id: 90,
                        anim_layer_idx: 0,
                        field: AnimLayerField::Blend,
                    }
            })
            .expect("blend field output");
        assert!((blend.value.as_f64().unwrap() - 0.42).abs() < 1e-5);
    }

    #[test]
    fn instance_override_script_drives_rate_and_colorn() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(95, "rain"));
        host.add_instance_override_script(
            95,
            "rate",
            "export function update() { return 0.35; }",
            json!(1.0),
            &serde_json::Map::new(),
        );
        host.add_instance_override_script(
            95,
            "colorn",
            "export function update() { return new Vec3(0.2, 0.4, 0.8); }",
            json!("1 1 1"),
            &serde_json::Map::new(),
        );
        let _ = host.finalize_init();
        let out = host.tick(
            0.05,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(95)],
        );
        let rate = out
            .iter()
            .find(|o| o.target == ScriptTarget::InstanceOverride { layer_id: 95, key: "rate".into() })
            .expect("rate override");
        assert!((rate.value.as_f64().unwrap() - 0.35).abs() < 1e-5);
        let col = out
            .iter()
            .find(|o| o.target == ScriptTarget::InstanceOverride { layer_id: 95, key: "colorn".into() })
            .expect("colorn override");
        let arr = col.value.as_array().unwrap();
        assert!((arr[2].as_f64().unwrap() - 0.8).abs() < 1e-5);
    }

    #[test]
    fn video_texture_script_pause_seek_and_rate() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        let mut s = seed(99, "vid");
        s.video_tex = Some(VideoTexSeed {
            duration: 12.0,
            current_time: 0.0,
            playing: true,
            rate: 1.0,
        });
        host.add_layer(&s);
        host.add_property_script(
            99,
            ScriptField::Visible,
            "let vid;\nexport function init() {\n  vid = thisLayer.getVideoTexture();\n  vid.pause();\n  vid.setCurrentTime(vid.duration * 0.25);\n  engine.setTimeout(() => { vid.setPlaybackRate(1.5); vid.play(); }, 100);\n}\nexport function update(v) {\n  shared.vidTime = vid.getCurrentTime();\n  return v;\n}",
            json!(true),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        let v0 = init_out
            .iter()
            .find(|o| o.target == ScriptTarget::VideoTexture { layer_id: 99 })
            .expect("initial video control");
        assert_eq!(v0.value.get("playing"), Some(&json!(false)));
        assert!((v0.value.get("seek").unwrap().as_f64().unwrap() - 3.0).abs() < 1e-4);

        // Next tick with real video_time update and no script change -> no redundant seek emission!
        let mut f = frame(99);
        f.video_time = Some((3.0, 12.0));
        let out1 = host.tick(
            0.05,
            0.05,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f.clone()],
        );
        assert!(out1.iter().all(|o| o.target != ScriptTarget::VideoTexture { layer_id: 99 }));

        // t=0.15: timeout resumes playback at rate=1.5 with seek == null.
        let out2 = host.tick(
            0.15,
            0.05,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f],
        );
        let v2 = out2
            .iter()
            .find(|o| o.target == ScriptTarget::VideoTexture { layer_id: 99 })
            .expect("resumed video control");
        assert_eq!(v2.value.get("playing"), Some(&json!(true)));
        assert!((v2.value.get("rate").unwrap().as_f64().unwrap() - 1.5).abs() < 1e-4);
        assert_eq!(v2.value.get("seek"), Some(&Value::Null));
    }

    #[test]
    fn cursor_events_hover_click_drag_and_solid_sync() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        host.add_layer(&seed(101, "btn"));
        let mut drag_seed = seed(102, "drag");
        drag_seed.origin = [500.0, 400.0, 0.0];
        host.add_layer(&drag_seed);
        host.add_layer(&seed(103, "plain"));

        host.add_property_script(
            101,
            ScriptField::Alpha,
            "export function cursorEnter(event) { thisLayer.visible = true; }\nexport function cursorLeave(event) { thisLayer.visible = false; }\nexport function cursorClick(event) { thisLayer.solid = false; }\nexport function update(v) { shared.down = input.cursorLeftDown; return v; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        host.add_property_script(
            102,
            ScriptField::Origin,
            "let dragging = false, offset;\nexport function cursorDown(event) { dragging = true; offset = thisLayer.origin.subtract(event.worldPosition); }\nexport function cursorMove(event) { if (dragging) thisLayer.origin = event.worldPosition.add(offset); }\nexport function cursorUp(event) { dragging = false; }",
            json!("500 400 0"),
            &serde_json::Map::new(),
            None,
        );
        host.add_property_script(
            103,
            ScriptField::Alpha,
            "export function update(v) { return v; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let _ = host.finalize_init();

        assert!(host.has_cursor_layers());
        assert!(host.is_cursor_layer(101));
        assert!(host.is_cursor_layer(102));
        assert!(!host.is_cursor_layer(103));

        // Frame 1: enter btn, down on drag at (510, 410) with mouse_down=true.
        let mut f_btn = frame(101);
        f_btn.visible = false;
        let mut f_drag = frame(102);
        f_drag.origin = [500.0, 400.0, 0.0];
        let f_plain = frame(103);
        let ev1 = [
            CursorEventInput {
                layer_id: 101,
                kind: CursorEventKind::Enter,
                world_pos: [100.0, 100.0],
                screen_pos: [0.1, 0.1],
                local_pos: [0.0, 0.0],
            },
            CursorEventInput {
                layer_id: 102,
                kind: CursorEventKind::Down,
                world_pos: [510.0, 410.0],
                screen_pos: [0.5, 0.5],
                local_pos: [10.0, 10.0],
            },
        ];
        let out1 = host.tick_with_input(
            0.1,
            0.016,
            0.5,
            [510.0, 410.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f_btn.clone(), f_drag.clone(), f_plain.clone()],
            [0.5, 0.5],
            true,
            &ev1,
            &[],
        );
        assert!(out1.iter().any(|o| {
            o.target == ScriptTarget::Layer { id: 101, field: ScriptField::Visible }
                && o.value == json!(true)
        }));
        f_btn.visible = true;

        // Frame 2: drag move to (545, 430) -> origin becomes (535, 420).
        let ev2 = [CursorEventInput {
            layer_id: 102,
            kind: CursorEventKind::Move,
            world_pos: [545.0, 430.0],
            screen_pos: [0.52, 0.52],
            local_pos: [45.0, 30.0],
        }];
        let out2 = host.tick_with_input(
            0.12,
            0.016,
            0.5,
            [545.0, 430.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f_btn.clone(), f_drag.clone(), f_plain.clone()],
            [0.52, 0.52],
            true,
            &ev2,
            &[],
        );
        let moved = out2
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 102, field: ScriptField::Origin })
            .expect("dragged origin output");
        let arr = moved.value.as_array().unwrap();
        assert!((arr[0].as_f64().unwrap() - 535.0).abs() < 1e-4);
        assert!((arr[1].as_f64().unwrap() - 420.0).abs() < 1e-4);
        f_drag.origin = [535.0, 420.0, 0.0];

        // Frame 3: leave btn + click btn -> visible=false and solid=false emitted.
        let ev3 = [
            CursorEventInput {
                layer_id: 101,
                kind: CursorEventKind::Leave,
                world_pos: [0.0, 0.0],
                screen_pos: [0.0, 0.0],
                local_pos: [-100.0, -100.0],
            },
            CursorEventInput {
                layer_id: 101,
                kind: CursorEventKind::Click,
                world_pos: [100.0, 100.0],
                screen_pos: [0.1, 0.1],
                local_pos: [0.0, 0.0],
            },
        ];
        let out3 = host.tick_with_input(
            0.14,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[f_btn, f_drag, f_plain],
            [0.0, 0.0],
            false,
            &ev3,
            &[],
        );
        assert!(out3.iter().any(|o| {
            o.target == ScriptTarget::Layer { id: 101, field: ScriptField::Visible }
                && o.value == json!(false)
        }));
        assert!(out3.iter().any(|o| {
            o.target == ScriptTarget::LayerSolid { id: 101 } && o.value == json!(false)
        }));
    }

    #[test]
    fn media_events_dispatch_all_callbacks_and_colors() {
        let mut host = SceneScripts::new(&serde_json::Map::new()).expect("host");
        for (id, name) in [(201, "title_lbl"), (202, "panel"), (203, "tint"), (204, "bar"), (205, "cover")] {
            host.add_layer(&seed(id, name));
        }
        host.add_effect(&EffectSeed {
            layer_id: 205,
            effect_idx: 0,
            id: Some(901),
            name: "blend".into(),
            visible: true,
        });

        host.add_property_script(
            201,
            ScriptField::Text,
            "let cur = ''; export function mediaPropertiesChanged(e) { cur = e.artist + ' - ' + e.title; } export function update() { return cur; }",
            json!(""),
            &serde_json::Map::new(),
            None,
        );
        host.add_property_script(
            202,
            ScriptField::Visible,
            "let en = false, pl = false;\nexport function mediaStatusChanged(e) { en = e.enabled; return en && pl; }\nexport function mediaPlaybackChanged(e) { pl = e.state === MediaPlaybackEvent.PLAYBACK_PLAYING; return en && pl; }",
            json!(false),
            &serde_json::Map::new(),
            None,
        );
        host.add_property_script(
            203,
            ScriptField::Color,
            "export function mediaThumbnailChanged(e) { if (!e.hasThumbnail) return new Vec3(0, 0, 0); return e.primaryColor.mix(e.textColor, 0.5); }",
            json!("1 1 1"),
            &serde_json::Map::new(),
            None,
        );
        host.add_property_script(
            204,
            ScriptField::Scale,
            "let ratio = 0;\nexport function mediaTimelineChanged(e) { ratio = e.duration > 0 ? e.position / e.duration : 0; }\nexport function update() { return new Vec3(ratio, 1, 1); }",
            json!("0 1 1"),
            &serde_json::Map::new(),
            None,
        );
        host.add_effect_constant_script(
            205,
            0,
            0,
            "blend",
            "export function mediaThumbnailChanged(e) { if (e.hasThumbnail) thisObject.getAnimation().play(); }",
            json!(0.0),
            &serde_json::Map::new(),
            Some(&PropAnimSeed {
                name: "reveal".into(),
                fps: 30.0,
                length: 30.0,
                mode: "single",
                start_paused: true,
            }),
        );

        let _ = host.finalize_init();
        assert!(host.uses_media());

        let events = [
            MediaEventInput::Status { enabled: true },
            MediaEventInput::Playback { state: 1 },
            MediaEventInput::Properties {
                title: "Dawn".into(),
                artist: "Miku".into(),
                album_title: "Sky".into(),
                album_artist: "Miku".into(),
                sub_title: String::new(),
                genres: "Electronic".into(),
                content_type: "music".into(),
            },
            MediaEventInput::Thumbnail {
                has_thumbnail: true,
                primary_color: [0.2, 0.4, 0.8],
                secondary_color: [0.1, 0.2, 0.5],
                tertiary_color: [0.7, 0.5, 0.2],
                text_color: [1.0, 1.0, 1.0],
                high_contrast_color: [1.0, 1.0, 1.0],
            },
            MediaEventInput::Timeline {
                position: 30.0,
                duration: 120.0,
            },
        ];
        let mut f202 = frame(202);
        f202.visible = false;
        let frames = [frame(201), f202, frame(203), frame(204), frame(205)];

        let out = host.tick_with_input(
            0.1,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &frames,
            [0.0, 0.0],
            false,
            &[],
            &events,
        );

        assert!(out.iter().any(|o| {
            o.target == ScriptTarget::Layer { id: 201, field: ScriptField::Text }
                && o.value == json!("Miku - Dawn")
        }));
        assert!(out.iter().any(|o| {
            o.target == ScriptTarget::Layer { id: 202, field: ScriptField::Visible }
                && o.value == json!(true)
        }));
        let color_out = out
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 203, field: ScriptField::Color })
            .expect("mediaThumbnailChanged color output");
        let c_arr = color_out.value.as_array().unwrap();
        assert!((c_arr[0].as_f64().unwrap() - 0.6).abs() < 1e-4);
        assert!((c_arr[1].as_f64().unwrap() - 0.7).abs() < 1e-4);
        assert!((c_arr[2].as_f64().unwrap() - 0.9).abs() < 1e-4);

        let scale_out = out
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 204, field: ScriptField::Scale })
            .expect("mediaTimelineChanged scale output");
        assert!((scale_out.value[0].as_f64().unwrap() - 0.25).abs() < 1e-4);

        let anim_ctrl = out
            .iter()
            .find(|o| {
                o.target
                    == ScriptTarget::EffectConstAnimControl {
                        layer_id: 205,
                        effect_idx: 0,
                        pass_idx: 0,
                        key: "blend".to_string(),
                    }
            })
            .expect("mediaThumbnailChanged triggers effect animation play()");
        assert_eq!(anim_ctrl.value.get("playing"), Some(&json!(true)));
    }

    #[test]
    fn sound_layer_play_pause_stop_is_playing_and_volume() {
        let mut props = serde_json::Map::new();
        props.insert("play".into(), json!(true));
        let mut host = SceneScripts::new(&props).expect("host");

        let mut snd_seed = seed(301, "theme.mp3");
        snd_seed.kind = "sound".into();
        snd_seed.sound = Some(SoundSeed {
            volume: 0.75,
            playing: false,
        });
        host.add_layer(&snd_seed);
        host.add_layer(&seed(302, "controller"));
        host.link_parents();

        // Direct `volume.script` with duplicate `export function update` (like Workshop 3432999904).
        let mut sp = serde_json::Map::new();
        sp.insert("play".into(), json!(true));
        host.add_property_script(
            301,
            ScriptField::Volume,
            "'use strict';
export function update(value) { return value; }
export var scriptProperties = createScriptProperties().addCheckbox({ name: 'play', value: false }).finish();
export function update(value) {
  if (scriptProperties.play && !thisLayer.isPlaying()) { thisLayer.play(); }
  else if (!scriptProperties.play && thisLayer.isPlaying()) { thisLayer.pause(); }
  return value * 0.8;
}",
            json!(1.0),
            &sp,
            None,
        );

        // Controller enumerating sound layers and calling getSound()/pause()/stop()/volume = v (like 3346831410).
        host.add_property_script(
            302,
            ScriptField::Alpha,
            "'use strict';
var phase = 0;
export function update(value) {
  phase++;
  thisScene.enumerateLayers().forEach(function (l) {
    if (l.name.indexOf('.mp3') >= 0) {
      if (phase === 2) {
        l.getSound().volume = 0.25;
        l.pause();
      } else if (phase === 3) {
        l.stop();
      }
    }
  });
  return value;
}",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );

        let mut f_snd = frame(301);
        f_snd.sound = Some((0.75, false));
        let f_ctrl = frame(302);

        // Tick 1: sound script triggers play() and scales volume 0.75 -> 0.6.
        let out1 = host.tick(0.0, 0.016, 0.5, [0.0; 2], [1920.0, 1080.0], [1920.0, 1080.0], None, &[f_snd.clone(), f_ctrl.clone()]);
        assert_eq!(host.failures(), 0, "sound script failed: {:?}", host.errors());
        let sc1 = out1
            .iter()
            .find(|o| o.target == ScriptTarget::SoundControl { layer_id: 301 })
            .expect("play sound_ctrl emitted");
        assert_eq!(sc1.value.get("action"), Some(&json!("play")));
        let vol1 = out1
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 301, field: ScriptField::Volume })
            .expect("volume output emitted");
        assert!((vol1.value.as_f64().unwrap() - 0.6).abs() < 1e-4);

        // Tick 2: controller calls pause() and sets volume = 0.25 via getSound().
        f_snd.sound = Some((0.6, true));
        let out2 = host.tick(0.016, 0.016, 0.5, [0.0; 2], [1920.0, 1080.0], [1920.0, 1080.0], None, &[f_snd.clone(), f_ctrl.clone()]);
        let sc2 = out2
            .iter()
            .find(|o| o.target == ScriptTarget::SoundControl { layer_id: 301 })
            .expect("pause sound_ctrl emitted");
        assert_eq!(sc2.value.get("action"), Some(&json!("pause")));
        let vol2 = out2
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 301, field: ScriptField::Volume })
            .expect("controller volume output emitted");
        assert!((vol2.value.as_f64().unwrap() - 0.25).abs() < 1e-4);

        // Tick 3: Rust syncs Single EOF (`playing: false`) -> controller calls stop().
        f_snd.sound = Some((0.25, false));
        let out3 = host.tick(0.032, 0.016, 0.5, [0.0; 2], [1920.0, 1080.0], [1920.0, 1080.0], None, &[f_snd, f_ctrl]);
        // Note: on Tick 3 scriptProperties.play is true and LayerFrame said playing=false, so 301 calls play() first, then 302 calls stop()!
        let sc3 = out3
            .iter()
            .find(|o| o.target == ScriptTarget::SoundControl { layer_id: 301 })
            .expect("stop sound_ctrl emitted");
        assert_eq!(sc3.value.get("action"), Some(&json!("stop")));
    }

    #[test]
    fn local_storage_persists_primitives_and_vectors_across_hosts() {
        let props = serde_json::Map::new();
        let mut h1 = SceneScripts::new(&props).expect("host1");
        h1.add_layer(&seed(401, "saved"));
        h1.link_parents();
        h1.add_property_script(
            401,
            ScriptField::Alpha,
            "'use strict';
export function init(value) {
  localStorage.set('pos', new Vec3(12, 34, 56));
  localStorage.set('flag', true);
  localStorage.set('count', 7);
  localStorage.set('temp', 'x');
  localStorage.delete('temp');
  return value;
}
export function update(value) { return value; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let _ = h1.finalize_init();
        assert_eq!(h1.failures(), 0, "host1 failed: {:?}", h1.errors());
        let snapshot = h1.take_storage_update().expect("storage update emitted");

        let mut h2 = SceneScripts::new_with_storage(&props, Some(&snapshot)).expect("host2");
        h2.add_layer(&seed(402, "loaded"));
        h2.link_parents();
        h2.add_property_script(
            402,
            ScriptField::Origin,
            "'use strict';
export function update(value) {
  var p = localStorage.get('pos');
  if (!(p instanceof Vec3)) throw new Error('expected Vec3 from localStorage');
  if (localStorage.get('flag') !== true) throw new Error('expected flag true');
  if (localStorage.get('count') !== 7) throw new Error('expected count 7');
  if (localStorage.get('temp') !== undefined) throw new Error('expected deleted key to be undefined');
  if (localStorage.LOCATION_SCREEN !== 0 || localStorage.LOCATION_GLOBAL !== 1) throw new Error('bad LOCATION_*');
  return p;
}",
            json!([0.0, 0.0, 0.0]),
            &serde_json::Map::new(),
            None,
        );
        let out = h2.tick(
            0.016,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(402)],
        );
        assert_eq!(h2.failures(), 0, "host2 failed: {:?}", h2.errors());
        let orig = out
            .iter()
            .find(|o| o.target == ScriptTarget::Layer { id: 402, field: ScriptField::Origin })
            .expect("origin restored from persisted Vec3");
        assert_eq!(prop_vec3(&orig.value, [0.0; 3]), [12.0, 34.0, 56.0]);
    }

    #[test]
    fn transform_matrix_composes_parents_and_text_alignment_props_sync() {
        let props = serde_json::Map::new();
        let mut host = SceneScripts::new(&props).expect("host");
        let mut parent = seed(501, "parent");
        parent.origin = [100.0, 200.0, 0.0];
        parent.angles = [0.0, 0.0, 90.0];
        parent.scale = [2.0, 3.0, 1.0];

        let mut child = seed(502, "child");
        child.kind = "text".into();
        child.parent = Some(501);
        child.origin = [10.0, 5.0, 0.0];

        host.add_layer(&parent);
        host.add_layer(&child);
        host.link_parents();
        host.add_property_script(
            502,
            ScriptField::Alpha,
            "'use strict';
export function init(value) {
  var mat = thisLayer.getTransformMatrix().m;
  if (Math.abs(mat[12] - 85) > 1e-3 || Math.abs(mat[13] - 220) > 1e-3) {
    throw new Error('bad world origin: ' + mat[12] + ',' + mat[13]);
  }
  if (Math.abs(mat[1] - 2) > 1e-3 || Math.abs(mat[4] + 3) > 1e-3) {
    throw new Error('bad world basis: ' + mat[0] + ',' + mat[1] + ',' + mat[4] + ',' + mat[5]);
  }
  thisLayer.origin = new Vec3(999, 888, 0);
  if (Math.abs(thisLayer.originalOrigin.x - 10) > 1e-3 || Math.abs(thisLayer.originalOrigin.y - 5) > 1e-3) {
    throw new Error('originalOrigin mutated: ' + thisLayer.originalOrigin.x);
  }
  thisLayer.origin = new Vec3(10, 5, 0);
  thisLayer.horizontalalign = 'right';
  thisLayer.verticalalign = 'bottom';
  thisLayer.pointsize = 48;
  thisLayer.font = 'fonts/custom.ttf';
  thisLayer.alignment = 'left';
  return value;
}
export function update(value) { return value; }",
            json!(1.0),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        assert_eq!(host.failures(), 0, "failed: {:?}", host.errors());

        let find_field = |field: ScriptField| {
            init_out
                .iter()
                .find(|o| o.target == ScriptTarget::Layer { id: 502, field })
                .map(|o| o.value.clone())
        };
        assert_eq!(find_field(ScriptField::HorizontalAlign), Some(json!("right")));
        assert_eq!(find_field(ScriptField::VerticalAlign), Some(json!("bottom")));
        assert_eq!(find_field(ScriptField::Font), Some(json!("fonts/custom.ttf")));
        assert_eq!(find_field(ScriptField::Alignment), Some(json!("left")));
        let ps = find_field(ScriptField::PointSize)
            .and_then(|v| v.as_f64())
            .expect("pointsize output");
        assert!((ps - 48.0).abs() < 1e-3);
    }

    #[test]
    fn scene_create_sort_and_destroy_layer_emit_ordered_ops() {
        let props = serde_json::Map::new();
        let mut host = SceneScripts::new(&props).expect("host");
        let mut bg = seed(601, "bg");
        bg.image = "models/bg.json".into();
        let mut main_l = seed(602, "clock");
        main_l.kind = "text".into();
        main_l.image = "models/clock.json".into();
        main_l.origin = [500.0, 300.0, 0.0];
        host.add_layer(&bg);
        host.add_layer(&main_l);
        host.link_parents();
        host.add_property_script(
            602,
            ScriptField::Visible,
            "'use strict';
let shadow = null;
export function init(value) {
  let cfg = thisScene.getInitialLayerConfig(thisLayer);
  if (cfg.image !== 'models/clock.json') throw new Error('missing initial image: ' + cfg.image);
  shadow = thisScene.createLayer({
    type: 'solid',
    name: 'clock_shadow',
    scale: new Vec2(4, 120),
    origin: new Vec3(10, 20, 0),
    color: '0 0 0'
  });
  shadow.visible = false;
  shadow.origin = thisLayer.origin;
  thisScene.sortLayer(shadow, thisScene.getLayerIndex(thisLayer));
  if (thisScene.getLayerIndex(shadow) !== 1 || thisScene.getLayerIndex(thisLayer) !== 2) {
    throw new Error('unexpected sort indices: shadow=' + thisScene.getLayerIndex(shadow) + ' self=' + thisScene.getLayerIndex(thisLayer));
  }
  return value;
}
export function update(value) {
  if (shadow) {
    thisScene.destroyLayer(shadow);
    shadow = null;
  }
  return value;
}",
            json!(true),
            &serde_json::Map::new(),
            None,
        );
        let init_out = host.finalize_init();
        assert_eq!(host.failures(), 0, "init failed: {:?}", host.errors());

        let create_pos = init_out
            .iter()
            .position(|o| matches!(o.target, ScriptTarget::SceneCreateLayer { .. }))
            .expect("createLayer op");
        let sort_pos = init_out
            .iter()
            .position(|o| matches!(o.target, ScriptTarget::SceneSortLayer { .. }))
            .expect("sortLayer op");
        assert!(create_pos < sort_pos, "create must precede sort");

        let dyn_id = match init_out[create_pos].target {
            ScriptTarget::SceneCreateLayer { id } => id,
            _ => unreachable!(),
        };
        assert_eq!(prop_vec3(&init_out[create_pos].value["scale"], [0.0; 3]), [4.0, 120.0, 1.0]);
        assert_eq!(init_out[sort_pos].value, json!(1));
        let vis_pos = init_out
            .iter()
            .position(|o| {
                o.target
                    == ScriptTarget::Layer {
                        id: dyn_id,
                        field: ScriptField::Visible,
                    }
            })
            .expect("initial post-create visible");
        assert!(sort_pos < vis_pos, "ops must precede post-create field writes");
        assert_eq!(init_out[vis_pos].value, json!(false));

        let tick_out = host.tick(
            0.016,
            0.016,
            0.5,
            [0.0, 0.0],
            [1920.0, 1080.0],
            [1920.0, 1080.0],
            None,
            &[frame(601), frame(602)],
        );
        assert_eq!(host.failures(), 0, "tick failed: {:?}", host.errors());
        assert!(tick_out
            .iter()
            .any(|o| o.target == ScriptTarget::SceneDestroyLayer { id: dyn_id }));
    }

    /// Compile and tick every SceneScript (layer properties, effect visibility,
    /// effect constant shaders, puppet animation layers, and particle instance
    /// overrides) in the local Wallpaper Engine Workshop library.
    /// Ignored by default: needs the workshop files.
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
        let mut layer_bindings = 0usize;
        let mut effect_vis_bindings = 0usize;
        let mut effect_const_bindings = 0usize;
        let mut anim_layer_bindings = 0usize;
        let mut inst_ov_bindings = 0usize;
        let mut audio_scenes = 0usize;
        let mut failures = 0usize;
        let mut reports: Vec<String> = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&root) else {
            eprintln!("no workshop library at {root}");
            return;
        };
        let mut spectrum = [0.0f32; 64];
        for (i, s) in spectrum.iter_mut().enumerate() {
            *s = ((i as f32) / 64.0) * 0.5;
        }
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
                host.add_layer(&LayerSeed {
                    id,
                    name: obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    kind: "image".into(),
                    visible: true,
                    solid: true,
                    alpha: 1.0,
                    origin: vec("origin", [0.0; 3]),
                    scale: vec("scale", [1.0; 3]),
                    angles: vec("angles", [0.0; 3]),
                    color: vec("color", [1.0; 3]),
                    size: [100.0, 100.0],
                    align: obj
                        .get("alignment")
                        .and_then(|v| v.as_str())
                        .unwrap_or("center")
                        .to_string(),
                    horizontal_align: obj
                        .get("horizontalalign")
                        .and_then(|v| v.as_str())
                        .unwrap_or("center")
                        .to_string(),
                    vertical_align: obj
                        .get("verticalalign")
                        .and_then(|v| v.as_str())
                        .unwrap_or("center")
                        .to_string(),
                    point_size: obj
                        .get("pointsize")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(32.0) as f32,
                    font: obj
                        .get("font")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    image: obj
                        .get("image")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    text: String::new(),
                    parent,
                    tex_anim: None,
                    video_tex: None,
                    sound: obj.get("sound").map(|_| SoundSeed {
                        volume: 1.0,
                        playing: !obj
                            .get("startsilent")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    }),
                });
                if let Some(effects) = obj.get("effects").and_then(|e| e.as_array()) {
                    for (eidx, ef) in effects.iter().enumerate() {
                        host.add_effect(&EffectSeed {
                            layer_id: id,
                            effect_idx: eidx,
                            id: ef.get("id").and_then(|v| v.as_i64()),
                            name: ef
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            visible: ef.get("visible").and_then(|v| v.as_bool()).unwrap_or(true),
                        });
                    }
                }
                if let Some(als) = obj.get("animationlayers").and_then(|a| a.as_array()) {
                    for (aidx, al) in als.iter().enumerate() {
                        host.add_anim_layer(&AnimLayerSeed {
                            layer_id: id,
                            anim_layer_idx: aidx,
                            id: al.get("id").and_then(|v| v.as_i64()),
                            name: al
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            visible: al.get("visible").and_then(|v| v.as_bool()).unwrap_or(true),
                            blend: al.get("blend").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32,
                            rate: al.get("rate").and_then(|v| v.as_f64()).unwrap_or(1.0) as f32,
                            frame_count: 300,
                            fps: 30.0,
                            duration: 10.0,
                            mode: "loop",
                        });
                    }
                }
                frames.push(LayerFrame {
                    id,
                    visible: true,
                    alpha: 1.0,
                    origin: vec("origin", [0.0; 3]),
                    scale: vec("scale", [1.0; 3]),
                    angles: vec("angles", [0.0; 3]),
                    color: vec("color", [1.0; 3]),
                    text: String::new(),
                    video_time: None,
                    sound: obj.get("sound").map(|_| (1.0, true)),
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
                    (ScriptField::Volume, "volume"),
                ] {
                    let Some(def) = obj.get(key) else { continue };
                    let Some(script) = def.get("script").and_then(|s| s.as_str()) else {
                        continue;
                    };
                    let sp = def
                        .get("scriptproperties")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    let initial = def.get("value").cloned().unwrap_or(Value::Null);
                    let pa = super::super::keytrack::KeyTrack::parse(def)
                        .map(|t| PropAnimSeed::from_track(&t));
                    host.add_property_script(id, field, script, initial, &sp, pa.as_ref());
                    layer_bindings += 1;
                }
                if let Some(effects) = obj.get("effects").and_then(|e| e.as_array()) {
                    for (eidx, ef) in effects.iter().enumerate() {
                        if let Some(vis) = ef.get("visible") {
                            if let Some(script) = vis.get("script").and_then(|s| s.as_str()) {
                                let sp = vis
                                    .get("scriptproperties")
                                    .and_then(|v| v.as_object())
                                    .cloned()
                                    .unwrap_or_default();
                                let initial =
                                    vis.get("value").and_then(|v| v.as_bool()).unwrap_or(true);
                                host.add_effect_visible_script(id, eidx, script, initial, &sp);
                                effect_vis_bindings += 1;
                            }
                        }
                        if let Some(passes) = ef.get("passes").and_then(|p| p.as_array()) {
                            for (pidx, pass) in passes.iter().enumerate() {
                                let Some(csv) = pass
                                    .get("constantshadervalues")
                                    .and_then(|c| c.as_object())
                                else {
                                    continue;
                                };
                                for (ck, cv) in csv {
                                    let Some(script) = cv.get("script").and_then(|s| s.as_str())
                                    else {
                                        continue;
                                    };
                                    let sp = cv
                                        .get("scriptproperties")
                                        .and_then(|v| v.as_object())
                                        .cloned()
                                        .unwrap_or_default();
                                    let initial = cv.get("value").cloned().unwrap_or(json!(0.0));
                                    let pa = super::super::keytrack::KeyTrack::parse(cv)
                                        .map(|t| PropAnimSeed::from_track(&t));
                                    host.add_effect_constant_script(
                                        id, eidx, pidx, ck, script, initial, &sp, pa.as_ref(),
                                    );
                                    effect_const_bindings += 1;
                                }
                            }
                        }
                    }
                }
                if let Some(als) = obj.get("animationlayers").and_then(|a| a.as_array()) {
                    for (aidx, al) in als.iter().enumerate() {
                        for (field, key) in [
                            (AnimLayerField::Visible, "visible"),
                            (AnimLayerField::Blend, "blend"),
                            (AnimLayerField::Rate, "rate"),
                        ] {
                            let Some(def) = al.get(key) else { continue };
                            let Some(script) = def.get("script").and_then(|s| s.as_str()) else {
                                continue;
                            };
                            let sp = def
                                .get("scriptproperties")
                                .and_then(|v| v.as_object())
                                .cloned()
                                .unwrap_or_default();
                            let initial = def.get("value").cloned().unwrap_or_else(|| {
                                if field == AnimLayerField::Visible {
                                    json!(true)
                                } else {
                                    json!(1.0)
                                }
                            });
                            let pa = super::super::keytrack::KeyTrack::parse(def)
                                .map(|t| PropAnimSeed::from_track(&t));
                            host.add_anim_layer_script(
                                id,
                                aidx,
                                field,
                                script,
                                initial,
                                &sp,
                                pa.as_ref(),
                            );
                            anim_layer_bindings += 1;
                        }
                    }
                }
                if let Some(io) = obj.get("instanceoverride").and_then(|v| v.as_object()) {
                    for (ok, ov) in io {
                        let Some(script) = ov.get("script").and_then(|s| s.as_str()) else {
                            continue;
                        };
                        let sp = ov
                            .get("scriptproperties")
                            .and_then(|v| v.as_object())
                            .cloned()
                            .unwrap_or_default();
                        let initial = ov.get("value").cloned().unwrap_or(json!(1.0));
                        host.add_instance_override_script(id, ok, script, initial, &sp);
                        inst_ov_bindings += 1;
                    }
                }
            }
            scenes += 1;
            let _ = host.finalize_init();
            if host.uses_audio() {
                audio_scenes += 1;
            }
            for step in 0..3 {
                let _ = host.tick(
                    1.0 + step as f32 * 0.016,
                    0.016,
                    0.5,
                    [0.0, 0.0],
                    [1920.0, 1080.0],
                    [1920.0, 1080.0],
                    Some(&spectrum),
                    &frames,
                );
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
        let bindings = layer_bindings
            + effect_vis_bindings
            + effect_const_bindings
            + anim_layer_bindings
            + inst_ov_bindings;
        eprintln!(
            "workshop: {scenes} scenes ({audio_scenes} audio), {bindings} bindings (layer={layer_bindings}, eff_vis={effect_vis_bindings}, eff_const={effect_const_bindings}, anim_layer={anim_layer_bindings}, inst_ov={inst_ov_bindings}), {failures} failures\n{}",
            reports.join("\n")
        );
        assert!(bindings > 100, "expected the workshop library to be present");
    }
}
