//! Blue Archive chibi conventions: models from lihaohong6/BlueArchiveModels
//! and voice lines from bluearchive.wiki (`contrib/ba-voice.py`). The 3D
//! renderer (`render/model3d.rs`) and the sound code (`voice.rs`) are
//! generic; they use these only for models and files that follow the game's
//! naming.

use crate::render::model3d::ClipNames;
use crate::voice::{Cue, Effect};

/// A model is a Blue Archive chibi if it has the game's clip names.
pub fn is_chibi<'a>(mut clips: impl Iterator<Item = &'a str>) -> bool {
    clips.any(|c| c == "Cafe_Idle" || c == "Normal_Idle")
}

/// Clip names by role. The café set (idle, walk, reaction) is what the game
/// plays in the café; the rest are battle and formation clips that look
/// right on a desktop. Attack, reload and death clips are left out.
pub const CLIPS: ClipNames = ClipNames {
    idle: &["Cafe_Idle", "Normal_Idle", "idle"],
    walk: &["Cafe_Walk", "Move_Ing", "walk", "run", "move_ing", "move"],
    pat: &["Cafe_Reaction", "reaction", "react"],
    held: &["Formation_Pickup", "pickup", "pick"],
    more_idle: &["Formation_Idle", "Rearrange_Idle"],
    more_pat: &["Rearrange_Reaction"],
    run: &["Move_Ing", "Move_Ing01"],
    fall: &["Vital_Panic", "Vital_Panic01"],
    // victory pose then its ending; callsigns; the EX skill pose
    flourishes: &[
        ("Victory_Start", Some("Victory_End")),
        ("Victory_Start_Random", Some("Victory_End_Random")),
        ("Public01", None),
        ("Public01_globefish", None),
        ("Normal_Callsign", None),
        ("Move_Callsign", None),
        ("Stand_Callsign", None),
        ("Exs", None),
    ],
};

/// Faces: one mesh per mouth shape (`Mouth_<id>`), shown at morph weight 0
/// and switched by the clips. Happiest first: open grin, open smile, closed
/// smiles.
pub const MOUTH_PREFIX: &str = "Mouth_";
pub const HAPPY_MOUTHS: [&str; 6] = ["Mouth_701", "Mouth_703", "Mouth_700", "Mouth_705", "Mouth_704", "Mouth_405"];

/// Bones that sway with movement (hair, tails, skirts, cloth, ribbons...):
/// secondary bones are `bone_<part>_...`; props (bags, boards, fishing rods)
/// use the same prefix with their own part names and stay rigid.
pub fn is_sway_bone(name: &str) -> bool {
    const PARTS: [&str; 9] = ["hair", "twintail", "tail", "skirt", "cloth", "frill", "ribbon", "hood", "waiststrap"];
    let n = name.to_ascii_lowercase();
    n.strip_prefix("bone_").is_some_and(|rest| PARTS.iter().any(|p| rest.starts_with(p) || rest.contains(&format!("_{p}"))))
}

/// Weapon and shield meshes (by node or mesh name, lowercase).
pub fn is_weapon(name: &str) -> bool {
    name.contains("weapon") || name.contains("shield")
}

/// Voice line names for each cue, as in `<Student>_<line>_<n>.ogg`.
const VOICE_LINES: [(Cue, &[&str]); 8] = [
    (Cue::Hello, &["LogIn"]),
    (Cue::Pat, &["Lobby"]),
    (Cue::Hurt, &["Battle_Damage"]),
    (Cue::Victory, &["Battle_Victory"]),
    (Cue::Ex, &["ExSkill"]),
    (Cue::Gesture, &["Battle_Shout", "Formation_Select", "Battle_In"]),
    (Cue::Chase, &["Battle_Move"]),
    (Cue::Idle, &["Cafe_monolog"]),
];

/// Which cue a voice line is for, from its name without the trailing
/// numbers: it has to end in `_<line>`, so `Lobby_1` is a pat line but
/// `EventLobby_1`, `MemorialLobby_2_1` and `ExSkill_Level_1` aren't.
pub fn voice_cue(base: &str) -> Option<Cue> {
    VOICE_LINES
        .iter()
        .find(|(_, names)| names.iter().any(|n| base.ends_with(&format!("_{n}"))))
        .map(|(cue, _)| *cue)
}

/// The game's sound effect for each effect.
pub fn effect_file(effect: Effect) -> &'static str {
    match effect {
        Effect::PickUp => "SE_Cartoon_02.wav",
        Effect::Release => "SE_Cartoon_01.wav",
    }
}
