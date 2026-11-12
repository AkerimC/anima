//! The park: a window of its own where the characters can live, and the
//! chest they fill with what they find (wood, rope, nails, paint) for the
//! user to build things with.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// app_id of the park window.
pub const APP_ID: &str = "dev.anima.Park";

/// What can go in the chest: id, label. Pictures are props/materials/<id>.png.
pub const MATERIALS: [(&str, &str); 4] = [("wood", "Wood"), ("rope", "Rope"), ("nail", "Nails"), ("paint", "Paint")];

/// material -> who brought how many
pub type Chest = BTreeMap<String, BTreeMap<String, u32>>;

pub fn chest_path() -> PathBuf {
    crate::util::data_dir().join("chest.json")
}

pub fn read_chest() -> Chest {
    std::fs::read_to_string(chest_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// How many of `id` there are in all.
pub fn count(chest: &Chest, id: &str) -> u32 {
    chest.get(id).map_or(0, |by| by.values().sum())
}

/// `name` puts one `id` in the chest.
pub fn add(name: &str, id: &str) {
    let mut chest = read_chest();
    *chest.entry(id.to_string()).or_default().entry(name.to_string()).or_default() += 1;
    if let Ok(text) = serde_json::to_string_pretty(&chest) {
        let _ = crate::util::atomic_write(&chest_path(), text.as_bytes());
    }
}

pub fn picture(id: &str) -> PathBuf {
    crate::util::data_dir().join("props").join("materials").join(format!("{id}.png"))
}

/// Something built in the park: what, where (share of the park's width,
/// its middle), turned round, and how many pieces (a slide grows with them).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Thing {
    pub kind: String,
    // which window it's in ("park", "cafe", "farm"), and a number of its own
    #[serde(default = "park_place")]
    pub place: String,
    #[serde(default)]
    pub id: u64,
    pub x: f32,
    // how far up the meadow behind (share of the window's height; 0: the floor)
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub flip: bool,
    #[serde(default = "one")]
    pub pieces: u32,
}

fn one() -> u32 {
    1
}

fn park_place() -> String {
    "park".into()
}

/// The windows the characters can live in: id, window title.
pub const PLACES: [(&str, &str); 3] = [("park", "Anima Park"), ("cafe", "Anima Cafe"), ("farm", "Anima Farm")];

pub fn place_title(place: &str) -> &'static str {
    PLACES.iter().find(|p| p.0 == place).map_or("Anima Park", |p| p.1)
}

/// The park as saved: what's built, and whether building is free (trying out).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Park {
    #[serde(default)]
    pub things: Vec<Thing>,
    #[serde(default)]
    pub free: bool,
}

pub fn park_path() -> PathBuf {
    crate::util::data_dir().join("park.json")
}

pub fn read_park() -> Park {
    std::fs::read_to_string(park_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn write_park(p: &Park) {
    if let Ok(text) = serde_json::to_string_pretty(p) {
        let _ = crate::util::atomic_write(&park_path(), text.as_bytes());
    }
}

/// What can be built: id, label, cost, where.
pub type Recipe = (&'static str, &'static str, &'static [(&'static str, u32)], &'static [&'static str]);

const PARK: &[&str] = &["park"];
const INSIDE: &[&str] = &["park", "cafe"];
const FARM: &[&str] = &["farm"];
const ANY: &[&str] = &["park", "cafe", "farm"];

/// "slide_piece" makes a slide bigger.
pub const RECIPES: [Recipe; 19] = [
    ("chair", "Chair", &[("wood", 1), ("nail", 1)], INSIDE),
    ("bench", "Bench", &[("wood", 1), ("nail", 1), ("paint", 1)], ANY),
    ("table", "Table", &[("wood", 2), ("nail", 1), ("paint", 1), ("rope", 1)], INSIDE),
    ("stove", "Stove", &[("wood", 1), ("nail", 2), ("paint", 1)], INSIDE),
    ("counter", "Serving counter", &[("wood", 1), ("nail", 2), ("paint", 2)], &["cafe"]),
    ("swing", "Swing", &[("wood", 2), ("rope", 3), ("nail", 1)], PARK),
    ("seesaw", "Seesaw", &[("wood", 2), ("nail", 1), ("paint", 1), ("rope", 1)], PARK),
    ("slide", "Slide", &[("wood", 2), ("nail", 1), ("paint", 2), ("rope", 1)], PARK),
    ("slide_piece", "Slide piece", &[("wood", 1), ("nail", 1)], PARK),
    ("flowers", "Flower bed", &[("paint", 2), ("rope", 1)], ANY),
    ("tree", "Tree", &[("wood", 1), ("rope", 1)], &["park", "farm"]),
    ("lamp", "Lamp", &[("nail", 1), ("paint", 1), ("rope", 1)], ANY),
    ("sandbox", "Sandbox", &[("wood", 1), ("paint", 2), ("rope", 1)], PARK),
    ("stall", "Snack stall", &[("wood", 2), ("nail", 1), ("paint", 2), ("rope", 1)], PARK),
    ("carrots", "Carrot bed", &[("wood", 1), ("rope", 1)], FARM),
    ("wheat", "Wheat field", &[("rope", 2), ("nail", 1)], FARM),
    ("apple_tree", "Apple tree", &[("wood", 1), ("paint", 1), ("rope", 1)], FARM),
    ("coop", "Chicken coop", &[("wood", 2), ("nail", 1), ("paint", 1), ("rope", 2)], FARM),
    ("barn", "Cow barn", &[("wood", 2), ("nail", 2), ("paint", 2), ("rope", 2)], FARM),
];

pub fn recipe(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.0 == id)
}

pub fn affordable(chest: &Chest, cost: &[(&str, u32)]) -> bool {
    cost.iter().all(|&(m, k)| count(chest, m) >= k)
}

/// Takes `cost` out of the chest, from whoever brought the most first.
pub fn spend(cost: &[(&str, u32)]) -> bool {
    let mut chest = read_chest();
    if !affordable(&chest, cost) {
        return false;
    }
    for &(m, k) in cost {
        let by = chest.entry(m.to_string()).or_default();
        for _ in 0..k {
            if let Some(name) = by.iter().filter(|e| *e.1 > 0).max_by_key(|e| *e.1).map(|e| e.0.clone()) {
                if let Some(v) = by.get_mut(&name) {
                    *v -= 1;
                }
            }
        }
        by.retain(|_, v| *v > 0);
    }
    chest.retain(|_, by| !by.is_empty());
    if let Ok(text) = serde_json::to_string_pretty(&chest) {
        let _ = crate::util::atomic_write(&chest_path(), text.as_bytes());
    }
    true
}

/// Puts `cost` back (something taken down), as the park's own.
pub fn refund(cost: &[(&str, u32)]) {
    for &(m, k) in cost {
        for _ in 0..k {
            add("Park", m);
        }
    }
}

// Sizes, in character heights (`unit`), measured from the ground.

/// A slide of `pieces`: top of the ladder, and how far the chute reaches.
pub fn slide_size(pieces: u32) -> (f32, f32) {
    let p = pieces.clamp(1, 5) as f32 - 1.0;
    (0.75 + 0.3 * p, 1.1 + 0.45 * p)
}
pub const SWING_TOP: f32 = 1.6;
pub const SWING_ROPE: f32 = 1.2;
pub const SEESAW_HALF: f32 = 1.1;
pub const SEESAW_PIVOT: f32 = 0.3;
pub const SEESAW_TILT: f32 = 0.22;
pub const SEAT: f32 = 0.36;

/// How wide a thing is drawn (character heights).
pub fn width(t: &Thing) -> f32 {
    match t.kind.as_str() {
        "chair" => 0.45,
        "bench" => 1.0,
        "table" => 1.3,
        "stove" => 0.75,
        "swing" => 1.3,
        "seesaw" => SEESAW_HALF * 2.0 + 0.2,
        "slide" => 0.35 + slide_size(t.pieces).1,
        "flowers" => 0.8,
        "tree" => 1.2,
        "lamp" => 0.3,
        "sandbox" => 1.4,
        "carrots" => 1.0,
        "wheat" => 1.2,
        "apple_tree" => 1.3,
        "coop" => 1.1,
        "barn" => 1.7,
        "counter" => 1.5,
        "stall" => 1.2,
        _ => 0.5,
    }
}

/// How many can use it at once (0: just to look at).
pub fn room(kind: &str) -> usize {
    match kind {
        "chair" => 1,
        "bench" | "swing" | "seesaw" => 2,
        "stove" => 3,
        "table" | "slide" | "sandbox" => 4,
        "carrots" | "wheat" | "apple_tree" | "coop" | "barn" => 2,
        "tree" => 1,
        _ => 0,
    }
}

/// Character height in the park window's pixels, as the daemon last saw it.
pub fn view_path() -> PathBuf {
    crate::util::data_dir().join("park-view.json")
}

/// A character's height as a share of the park's width, if known.
pub fn read_unit_frac() -> Option<f32> {
    std::fs::read_to_string(view_path()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|v| v["unit_frac"].as_f64()).map(|u| u as f32)
}



// --- the farm: crops grow, hens lay, cows give milk (seconds)

pub const CROP_GROW: f64 = 360.0;
pub const APPLE_GROW: f64 = 480.0;
pub const EGG_EVERY: f64 = 180.0;
pub const MILK_EVERY: f64 = 240.0;
/// A tree (park or farm) drops wood: one log this often, up to three.
pub const WOOD_EVERY: f64 = 240.0;
/// The pond's share of the farm window, on the right; the pier runs out over it.
pub const POND: f32 = 0.13;
pub const PIER: (f32, f32) = (0.855, 0.975);
/// Little jetties up the pond's shore, for more anglers: their heights
/// (share of the window's height) and how far they reach into the water.
pub const JETTIES: [f32; 4] = [0.1, 0.19, 0.28, 0.37];
pub const JETTY: (f32, f32) = (1.0 - POND - 0.01, 1.0 - POND + 0.06);
/// How far up the meadow things can be put (share of the window's height).
pub const MEADOW: f32 = 0.45;

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// For each farm thing (by id): when it was planted or last emptied.
pub type Farm = BTreeMap<String, f64>;

pub fn farm_path() -> PathBuf {
    crate::util::data_dir().join("farm.json")
}

pub fn read_farm() -> Farm {
    std::fs::read_to_string(farm_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// `read_farm`, at most once every two seconds (it's asked every frame).
pub fn read_farm_cached() -> Farm {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Farm)>> = std::sync::Mutex::new(None);
    let mut c = CACHE.lock().unwrap();
    if c.as_ref().is_none_or(|(t, _)| t.elapsed().as_secs_f32() > 2.0) {
        *c = Some((std::time::Instant::now(), read_farm()));
    }
    c.as_ref().map(|x| x.1.clone()).unwrap_or_default()
}

pub fn write_farm(f: &Farm) {
    if let Ok(text) = serde_json::to_string_pretty(f) {
        let _ = crate::util::atomic_write(&farm_path(), text.as_bytes());
    }
}

/// How far along a farm thing is, 0..1 (1: ripe / something to collect),
/// and how many there are to take (eggs, milk).
pub fn farm_state(t: &Thing, farm: &Farm) -> (f32, u32) {
    let since = farm.get(&t.id.to_string()).copied().unwrap_or(0.0);
    let gone = (now() - since).max(0.0);
    match t.kind.as_str() {
        "carrots" | "wheat" => ((gone / CROP_GROW).min(1.0) as f32, u32::from(gone >= CROP_GROW)),
        "apple_tree" => ((gone / APPLE_GROW).min(1.0) as f32, if gone >= APPLE_GROW { 3 } else { 0 }),
        "coop" => {
            let k = ((gone / EGG_EVERY) as u32).min(3);
            (k as f32 / 3.0, k)
        }
        "barn" => {
            let k = ((gone / MILK_EVERY) as u32).min(2);
            (k as f32 / 2.0, k)
        }
        "tree" => {
            let k = ((gone / WOOD_EVERY) as u32).min(3);
            (k as f32 / 3.0, k)
        }
        _ => (0.0, 0),
    }
}

// --- the kitchen

/// What goes in the pot: id, label. Pictures are props/food/<id>.png.
pub const INGREDIENTS: [(&str, &str); 6] = [("fish", "Fish"), ("carrot", "Carrot"), ("wheat", "Wheat"), ("apple", "Apple"), ("egg", "Egg"), ("milk", "Milk")];

/// What can come out: id, label, what goes in (sorted as in INGREDIENTS).
pub const DISHES: [(&str, &str, &[&str]); 12] = [
    ("fish_soup", "Fish soup", &["fish", "carrot"]),
    ("pancakes", "Pancakes", &["wheat", "egg", "milk"]),
    ("apple_pie", "Apple pie", &["wheat", "apple"]),
    ("custard", "Custard", &["egg", "milk"]),
    ("bread", "Bread", &["wheat", "milk"]),
    ("fried_fish", "Fried fish", &["fish", "wheat"]),
    ("sushi", "Sushi", &["fish", "egg"]),
    ("salad", "Salad", &["carrot", "apple"]),
    ("carrot_cake", "Carrot cake", &["carrot", "wheat", "egg"]),
    ("cookies", "Cookies", &["wheat", "egg"]),
    ("ice_cream", "Ice cream", &["apple", "milk"]),
    ("stew", "Stew", &["fish", "carrot", "egg"]),
];

pub fn food_picture(id: &str) -> PathBuf {
    crate::util::data_dir().join("props").join("food").join(format!("{id}.png"))
}

pub fn animal_picture(kind: &str) -> PathBuf {
    crate::util::data_dir().join("props").join("animals").join(format!("{kind}.png"))
}

pub fn dish_label(id: &str) -> &'static str {
    if id == "burnt" {
        return "Something burnt";
    }
    DISHES.iter().find(|d| d.0 == id).map_or("Something", |d| d.1)
}

/// The dish these make, if any (order doesn't matter).
pub fn dish_of(mut stuff: Vec<&str>) -> Option<&'static str> {
    let rank = |x: &&str| INGREDIENTS.iter().position(|i| i.0 == *x).unwrap_or(99);
    stuff.sort_by_key(rank);
    DISHES.iter().find(|d| d.2 == stuff.as_slice()).map(|d| d.0)
}

pub type Pantry = BTreeMap<String, u32>;

pub fn pantry_path() -> PathBuf {
    crate::util::data_dir().join("pantry.json")
}

/// `read_pantry`, at most once a second (for checks made every frame).
pub fn pantry_total_cached() -> u32 {
    static C: std::sync::Mutex<Option<(std::time::Instant, u32)>> = std::sync::Mutex::new(None);
    let mut c = C.lock().unwrap();
    if c.is_none_or(|(t, _)| t.elapsed().as_secs_f32() > 1.0) {
        *c = Some((std::time::Instant::now(), read_pantry().values().sum()));
    }
    c.map_or(0, |x| x.1)
}

pub fn read_pantry() -> Pantry {
    std::fs::read_to_string(pantry_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_pantry(p: &Pantry) {
    if let Ok(text) = serde_json::to_string_pretty(p) {
        let _ = crate::util::atomic_write(&pantry_path(), text.as_bytes());
    }
}

pub fn add_food(id: &str, k: u32) {
    let mut p = read_pantry();
    *p.entry(id.to_string()).or_default() += k;
    write_pantry(&p);
}

/// Takes one of each; false (and nothing taken) if one is missing.
pub fn take_food(ids: &[&str]) -> bool {
    let mut p = read_pantry();
    if !ids.iter().all(|i| p.get(*i).copied().unwrap_or(0) >= 1) {
        return false;
    }
    for i in ids {
        if let Some(v) = p.get_mut(*i) {
            *v -= 1;
        }
    }
    p.retain(|_, v| *v > 0);
    write_pantry(&p);
    true
}

/// Recipes found so far, and mixes that went wrong.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct Book {
    #[serde(default)]
    pub found: BTreeMap<String, Found>,
    #[serde(default)]
    pub failed: Vec<Vec<String>>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct Found {
    pub by: String,
    pub times: u32,
}

pub fn book_path() -> PathBuf {
    crate::util::data_dir().join("recipes.json")
}

pub fn read_book() -> Book {
    std::fs::read_to_string(book_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn write_book(b: &Book) {
    if let Ok(text) = serde_json::to_string_pretty(b) {
        let _ = crate::util::atomic_write(&book_path(), text.as_bytes());
    }
}

// --- what's going on right now, for the windows to draw (in the runtime dir)

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct World {
    // swings swinging, seesaws rocking (thing indexes)
    #[serde(default)]
    pub busy: Vec<usize>,
    #[serde(default)]
    pub animals: Vec<Animal>,
    // stove id, "cooking" / "burnt" / "done", how far along 0..1
    #[serde(default)]
    pub stoves: Vec<(u64, String, f32)>,
    // table id, dish, how much is left 0..1
    #[serde(default)]
    pub dishes: Vec<(u64, String, f32)>,
    // a picnic in the park: where (share of its width), what, how much is left
    #[serde(default)]
    pub picnics: Vec<(f32, String, f32)>,
    // how hungry each one is, 0..1
    #[serde(default)]
    pub hunger: Vec<(String, f32)>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Animal {
    pub kind: String,
    // share of the window's width, facing left, how high it hops (unit)
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    pub left: bool,
    pub hop: f32,
}

pub fn world_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("anima-world.json")
}

pub fn read_world() -> World {
    std::fs::read_to_string(world_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

// --- jobs: earned over time by doing the work, not everyone gets one

/// id, label, where it's done, how many can have it.
pub const JOBS: [(&str, &str, &str, usize); 5] = [
    ("attendant", "Park attendant", "park", 1),
    ("chef", "Chef", "cafe", 1),
    ("waiter", "Waiter", "cafe", 2),
    ("farmer", "Farmer", "farm", 2),
    ("fisher", "Fisher", "farm", 2),
];

/// Work done to earn a job.
pub const JOB_AT: f32 = 20.0;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct Jobs {
    // name -> job -> work done
    #[serde(default)]
    pub points: BTreeMap<String, BTreeMap<String, f32>>,
    // name -> job
    #[serde(default)]
    pub jobs: BTreeMap<String, String>,
    // name -> job it helps out with part-time (its job's places were all taken)
    #[serde(default)]
    pub part: BTreeMap<String, String>,
}

/// Work done to be taken on as an apprentice (part-time).
pub const APPRENTICE_AT: f32 = 5.0;

/// How many can help out part-time in each job.
pub const PART_SLOTS: usize = 2;

pub fn jobs_path() -> PathBuf {
    crate::util::data_dir().join("jobs.json")
}

pub fn read_jobs() -> Jobs {
    std::fs::read_to_string(jobs_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

static JOBS_CACHE: std::sync::Mutex<Option<(std::time::Instant, Jobs)>> = std::sync::Mutex::new(None);

/// `read_jobs`, at most once every two seconds.
pub fn jobs_cached() -> Jobs {
    let mut c = JOBS_CACHE.lock().unwrap();
    if c.as_ref().is_none_or(|(t, _)| t.elapsed().as_secs_f32() > 2.0) {
        *c = Some((std::time::Instant::now(), read_jobs()));
    }
    c.as_ref().map(|x| x.1.clone()).unwrap_or_default()
}

/// Everyone who knows `job` (its own and those helping out) takes turns at
/// it: the job's places go round them a shift at a time, the rest help out.
fn shift_holds(j: &Jobs, job: &str, name: &str) -> bool {
    let mut holders: Vec<&String> = j.jobs.iter().chain(j.part.iter()).filter(|e| e.1 == job).map(|e| e.0).collect();
    holders.sort();
    let slots = JOBS.iter().find(|x| x.0 == job).map_or(1, |x| x.3);
    // room for all: apprentices work alongside
    if holders.len() <= slots {
        return holders.iter().any(|h| *h == name);
    }
    let len = crate::render::tuning::get().shift_len.max(60.0) as u64;
    let shift = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / len) as usize;
    (0..slots).any(|k| holders[(shift * slots + k) % holders.len()] == name)
}

fn with_jobs<T>(f: impl FnOnce(&Jobs) -> T) -> T {
    let mut c = JOBS_CACHE.lock().unwrap();
    if c.as_ref().is_none_or(|(t, _)| t.elapsed().as_secs_f32() > 2.0) {
        *c = Some((std::time::Instant::now(), read_jobs()));
    }
    match c.as_ref() {
        Some(x) => f(&x.1),
        None => f(&Jobs::default()),
    }
}

/// The job `name` is on shift for now. Asked many times a frame (cached).
pub fn job_of(name: &str) -> Option<String> {
    with_jobs(|j| {
        let job = j.jobs.get(name).or_else(|| j.part.get(name))?;
        shift_holds(j, job, name).then(|| job.clone())
    })
}

/// The job `name` helps out with while it's not its shift, if any.
pub fn part_of(name: &str) -> Option<String> {
    with_jobs(|j| {
        let job = j.jobs.get(name).or_else(|| j.part.get(name))?;
        (!shift_holds(j, job, name)).then(|| job.clone())
    })
}

pub fn job_label(id: &str) -> &'static str {
    JOBS.iter().find(|j| j.0 == id).map_or("", |j| j.1)
}

/// Jobs only for those in the team; a place come free goes to whoever
/// helps out with that job and has done the most of it.
pub fn tidy_jobs(team: &[String], prune: bool) -> Vec<String> {
    let mut j = read_jobs();
    let before = (j.jobs.clone(), j.part.clone());
    // (only once the team is surely all loaded)
    if prune && !team.is_empty() {
        j.jobs.retain(|name, _| team.contains(name));
        j.part.retain(|name, _| team.contains(name));
    }
    let mut told = Vec::new();
    for (id, _, _, most) in JOBS {
        while j.jobs.values().filter(|v| *v == id).count() < most {
            let pts = |name: &String| j.points.get(name).and_then(|p| p.get(id)).copied().unwrap_or(0.0);
            // only those who've done enough of it; apprentices wait
            let Some(name) = j.part.iter().filter(|p| p.1 == id && pts(p.0) >= JOB_AT).map(|p| p.0.clone()).max_by(|a, b| pts(a).total_cmp(&pts(b))) else { break };
            j.part.remove(&name);
            j.jobs.insert(name.clone(), id.to_string());
            told.push(format!("{name} is now the {}", job_label(id).to_lowercase()));
        }
    }
    // a job with someone at it takes on apprentices: those without a job who
    // have done most of it, so one alone doesn't keep all the work
    if !team.is_empty() {
        for (id, _, _, _) in JOBS {
            if !j.jobs.values().any(|v| v == id) {
                continue;
            }
            while j.part.values().filter(|v| *v == id).count() < PART_SLOTS {
                let pts = |name: &String| j.points.get(name).and_then(|p| p.get(id)).copied().unwrap_or(0.0);
                let free = team.iter().filter(|n| !j.jobs.contains_key(*n) && !j.part.contains_key(*n) && pts(n) >= APPRENTICE_AT);
                let Some(name) = free.max_by(|a, b| pts(a).total_cmp(&pts(b))).cloned() else { break };
                j.part.insert(name.clone(), id.to_string());
                told.push(format!("{name} helps out as {}", job_label(id).to_lowercase()));
            }
        }
    }
    if (j.jobs.clone(), j.part.clone()) != before {
        if let Ok(text) = serde_json::to_string_pretty(&j) {
            let _ = crate::util::atomic_write(&jobs_path(), text.as_bytes());
        }
        *JOBS_CACHE.lock().unwrap() = Some((std::time::Instant::now(), j));
    }
    told
}

/// `name` did some of `job`'s work. Returns the job if that just earned it.
pub fn add_work(name: &str, job: &str, k: f32) -> Option<String> {
    let mut j = read_jobs();
    *j.points.entry(name.to_string()).or_default().entry(job.to_string()).or_default() += k;
    let mut earned = None;
    if !j.jobs.contains_key(name) {
        let points = j.points.get(name).cloned().unwrap_or_default();
        let room = |j: &Jobs, id: &str| j.jobs.values().filter(|v| *v == id).count() < JOBS.iter().find(|x| x.0 == id).map_or(0, |x| x.3);
        // of the jobs with a place left, the one it does most, once it's done enough
        let best_open = points.iter().filter(|(id, _)| room(&j, id)).max_by(|a, b| a.1.total_cmp(b.1)).map(|e| (e.0.clone(), *e.1));
        if let Some((id, k)) = best_open.filter(|b| b.1 >= JOB_AT) {
            let _ = k;
            j.part.remove(name);
            j.jobs.insert(name.to_string(), id.clone());
            earned = Some(id);
        } else if !j.part.contains_key(name) {
            // all the places in what it does most are taken: it helps out part-time
            let part_room = |j: &Jobs, id: &str| j.part.values().filter(|v| *v == id).count() < PART_SLOTS;
            let best_full = points.iter().filter(|(id, k)| **k >= JOB_AT && !room(&j, id) && part_room(&j, id)).max_by(|a, b| a.1.total_cmp(b.1)).map(|e| e.0.clone());
            if let Some(id) = best_full {
                j.part.insert(name.to_string(), id.clone());
                earned = Some(format!("part:{id}"));
            }
        }
    }
    if let Ok(text) = serde_json::to_string_pretty(&j) {
        let _ = crate::util::atomic_write(&jobs_path(), text.as_bytes());
    }
    *JOBS_CACHE.lock().unwrap() = Some((std::time::Instant::now(), j));
    earned
}

// --- carrot coins: earned by bringing food to the pantry, spent at the cafe

/// What bringing one of these to the pantry earns.
pub fn food_worth(id: &str) -> u32 {
    match id {
        "fish" => 3,
        "egg" | "milk" => 2,
        _ => 1,
    }
}

/// What a dish costs at the cafe (once it has a chef).
pub fn dish_price(id: &str) -> u32 {
    DISHES.iter().find(|d| d.0 == id).map_or(5, |d| 3 * d.2.len() as u32 + 2)
}

pub type Wallets = BTreeMap<String, u32>;

pub fn wallets_path() -> PathBuf {
    crate::util::data_dir().join("wallets.json")
}

pub fn read_wallets() -> Wallets {
    std::fs::read_to_string(wallets_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write_wallets(w: &Wallets) {
    if let Ok(text) = serde_json::to_string_pretty(w) {
        let _ = crate::util::atomic_write(&wallets_path(), text.as_bytes());
    }
}

pub fn coins(name: &str) -> u32 {
    read_wallets().get(name).copied().unwrap_or(0)
}

pub fn earn(name: &str, k: u32) {
    let mut w = read_wallets();
    *w.entry(name.to_string()).or_default() += k;
    write_wallets(&w);
}

/// `from` pays `k`, shared out to `to` (who gets what share); false if it
/// hasn't got that much.
pub fn pay(from: &str, k: u32, to: &[(&str, f32)]) -> bool {
    let mut w = read_wallets();
    let have = w.get(from).copied().unwrap_or(0);
    if have < k {
        return false;
    }
    w.insert(from.to_string(), have - k);
    let mut left = k;
    for (n, (name, share)) in to.iter().enumerate() {
        let part = if n + 1 == to.len() { left } else { ((k as f32 * share).round() as u32).min(left) };
        left -= part;
        *w.entry(name.to_string()).or_default() += part;
    }
    write_wallets(&w);
    true
}

/// `from` pays up to `k` to `to`; returns what was paid.
pub fn pay_upto(from: &str, k: u32, to: &str) -> u32 {
    let k = k.min(coins(from));
    if k > 0 {
        pay(from, k, &[(to, 1.0)]);
    }
    k
}

/// Pay day's tax: of what each one has, a share of the part above each
/// step goes to the park. Returns who paid how much.
pub fn tax(steps: &[(u32, f32)]) -> Vec<(String, u32)> {
    let mut w = read_wallets();
    let mut paid = Vec::new();
    let mut total = 0;
    for (name, have) in w.iter_mut() {
        if name == "Park" {
            continue;
        }
        let due: u32 = steps.iter().map(|&(over, rate)| (have.saturating_sub(over) as f32 * rate).floor() as u32).sum::<u32>().min(*have);
        if due > 0 {
            *have -= due;
            total += due;
            paid.push((name.clone(), due));
        }
    }
    *w.entry("Park".to_string()).or_default() += total;
    write_wallets(&w);
    paid
}

// --- hunger, favourites, snacks, pay

/// Each one's favourite dish: always the same one for a name.
pub fn favorite(name: &str) -> &'static str {
    let h = name.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    DISHES[h as usize % DISHES.len()].0
}

/// How much a dish fills (taken off hunger, 0..1).
pub fn fills(dish: &str, favorite: bool) -> f32 {
    let base = match dish {
        "burnt" => 0.1,
        "snack" => 0.2,
        d => DISHES.iter().find(|x| x.0 == d).map_or(0.3, |x| 0.25 * x.2.len() as f32),
    };
    base + if favorite { 0.2 } else { 0.0 }
}

/// What a snack at the stall costs.
pub const SNACK_PRICE: u32 = 3;
/// What the stall sells: dish pictures.
pub const SNACKS: [&str; 2] = ["ice_cream", "cookies"];
