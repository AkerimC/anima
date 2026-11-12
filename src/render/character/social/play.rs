//! Playing in the park: down the slide, on the swing and the seesaw, sitting
//! on chairs and benches, talking round the table, cooking at the stove,
//! digging in the sandbox. What's built comes from the park window.

use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use super::*;
use crate::render::character::park::{self, Park, Thing};

mod dine;
mod host;
mod hunger;
mod shop;
mod places;
mod visit;

pub(super) struct Play {
    // which thing in the park list, and what it was when they started
    thing: usize,
    kind: String,
    riders: Vec<Rider>,
    t: f32,
    // how long this go lasts
    stay: f32,
    // at the stove: what's in the pot; carrying it: where from (strip px)
    meal: Option<Meal>,
    from: f32,
    // carrying it off the serving counter (its id), and is it the cook doing it
    counter: u64,
    by_cook: bool,
    // cooking for / taking the order of this diner (usize::MAX: nobody)
    diner: usize,
    // at the stall to buy something to keep, not a snack
    shop: bool,
}

#[derive(Clone)]
struct Meal {
    dish: String,
    new: bool,
    burnt: bool,
    stuff: Vec<String>,
}

/// A hen or a cow on the farm, belonging to a coop or barn (by id).
struct Beast {
    home: u64,
    kind: &'static str,
    x: f32,
    goal: f32,
    left: bool,
    wait: f32,
    hop: f32,
    // how far up the meadow (share of the window's height), and where it's going
    y: f32,
    goal_y: f32,
    // running from someone: how fast, for how long still
    flee: f32,
    flee_t: f32,
}

/// Animals, what's on the stoves and tables: kept here, drawn by the windows.
struct Live {
    beasts: Vec<Beast>,
    stoves: Vec<(u64, String, f32)>,
    dishes: Vec<(u64, String, f32)>,
    // picnics in the park: strip x, dish, how much is left
    picnics: Vec<(f32, String, f32)>,
    written: Option<Instant>,
    last: Option<park::World>,
    seed: u32,
}

static LIVE: Mutex<Live> = Mutex::new(Live { beasts: Vec::new(), stoves: Vec::new(), dishes: Vec::new(), picnics: Vec::new(), written: None, last: None, seed: 7 });

fn live_rand(l: &mut Live) -> f32 {
    l.seed ^= l.seed << 13;
    l.seed ^= l.seed >> 17;
    l.seed ^= l.seed << 5;
    (l.seed % 10000) as f32 / 10000.0
}

fn set_stove(id: u64, state: &str, k: f32) {
    let mut l = LIVE.lock().unwrap();
    l.stoves.retain(|s| s.0 != id);
    if !state.is_empty() {
        l.stoves.push((id, state.to_string(), k));
    }
}

fn dish_on(table: u64) -> Option<(String, f32)> {
    LIVE.lock().unwrap().dishes.iter().find(|d| d.0 == table).map(|d| (d.1.clone(), d.2))
}

/// What they're there for (the windows' second menu): 0 both, 1 only to
/// play (no hunger, no farming, fishing or cooking: they tire and nap
/// instead), 2 work first (games now and then).
pub(super) fn activity() -> u8 {
    tn().activity.round().clamp(0.0, 2.0) as u8
}

pub(super) fn playing_only() -> bool {
    activity() == 1
}

pub(super) fn working() -> bool {
    activity() == 2
}

/// The work things (farm, stove, stall, a tree's wood): none of it while only playing.
fn work_thing(kind: &str) -> bool {
    matches!(kind, "carrots" | "wheat" | "apple_tree" | "coop" | "barn" | "tree" | "stove" | "stall" | "counter")
}

/// The fun things in the park.
fn fun_thing(kind: &str) -> bool {
    matches!(kind, "slide" | "swing" | "seesaw" | "sandbox")
}

/// A thing's place's column: (x, width) on its workspace's strip.
fn column(place: &str) -> Option<(f32, f32)> {
    governor::place_rect(place).map(|p| (p.x, p.w))
}


#[derive(Clone, Copy)]
struct Rider {
    i: usize,
    phase: u8,
    t: f32,
    // goes down the slide this many times / swings this long
    n: u8,
    want: u8,
}

struct Cache {
    checked: Option<Instant>,
    mtime: Option<SystemTime>,
    park: Park,
    unit_written: f32,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { checked: None, mtime: None, park: Park { things: Vec::new(), free: false }, unit_written: 0.0 });

/// What's built, read again when the file changes (looked at once a second).
fn park_now() -> Park {
    let mut c = CACHE.lock().unwrap();
    if c.checked.is_none_or(|t| t.elapsed().as_secs_f32() >= 1.0) {
        c.checked = Some(Instant::now());
        let m = std::fs::metadata(park::park_path()).and_then(|m| m.modified()).ok();
        if m != c.mtime {
            c.mtime = m;
            c.park = park::read_park();
        }
    }
    c.park.clone()
}

impl CharacterOverlay {
    /// On the farm: the pier's stretch of floor (strip px), where they fish.
    pub(super) fn pier_of(&self, i: usize) -> Option<(f32, f32)> {
        self.place_here(i).filter(|p| p.place == "farm").map(|p| (p.x + park::PIER.0 * p.w, p.x + park::PIER.1 * p.w))
    }

    /// Where `i` can stand on dry ground (strip px): on the farm's workspace,
    /// short of the pond; elsewhere anywhere.
    pub(super) fn dry_land(&self, i: usize) -> (f32, f32) {
        match governor::place_rect("farm").filter(|p| p.ws == self.walk_ref(i).ws) {
            Some(p) if self.strip_px(i) >= p.x - 1.0 => (p.x, p.x + (1.0 - park::POND - 0.02) * p.w),
            _ => (f32::MIN, f32::MAX),
        }
    }

    /// Where `i` holds something in its hands (its middle, not its face), surface px.
    pub(super) fn hands_y(&self, i: usize) -> f32 {
        let rect = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        let h = (rect[3] - rect[1]).max(1.0);
        let o = self.actor_ref(i).opaque;
        let (top, bottom) = (rect[1] + h * o[1], rect[1] + h * o[2]);
        top + (bottom - top) * 0.62
    }

    /// The farm's pier, if the farm is on `i`'s workspace (it can walk there).
    pub(super) fn pier_near(&self, i: usize) -> Option<(f32, f32)> {
        governor::place_rect("farm").filter(|p| p.ws == self.walk_ref(i).ws).map(|p| (p.x + park::PIER.0 * p.w, p.x + park::PIER.1 * p.w))
    }

    /// `i` brings `k` of `id` to the pantry, and is paid for it in carrot coins.
    pub(super) fn bring_food(&mut self, i: usize, id: &str, k: u32) {
        // early days, nowhere to cook: a hungry one eats it there and then
        if self.food_stage() == 0 && self.hungry(i) {
            log::debug!("{} eats the {id} raw", self.character_name(i));
            self.eat(i, "snack");
            let name = self.character_name(i);
            park::pay_upto("Park", park::food_worth(id) * k, &name);
            return;
        }
        park::add_food(id, k);
        // the park buys it, with what's in the piggy bank
        let name = self.character_name(i);
        park::pay_upto("Park", park::food_worth(id) * k, &name);
    }

    /// `i` has this job.
    pub(super) fn is_job(&self, i: usize, job: &str) -> bool {
        park::job_of(&self.character_name(i)).as_deref() == Some(job)
    }

    /// Helps out with `job` part-time.
    pub(super) fn is_part(&self, i: usize, job: &str) -> bool {
        park::part_of(&self.character_name(i)).as_deref() == Some(job)
    }

    /// May `i` do `job`'s work? Yes if it's its job or it helps out with it;
    /// no if it has another; with neither, only while nobody has that one.
    pub(super) fn may_work(&self, i: usize, job: &str) -> bool {
        let name = self.character_name(i);
        match (park::job_of(&name), park::part_of(&name)) {
            (Some(j), _) => j == job,
            (None, Some(p)) => p == job,
            (None, None) => {
                let jobs = park::jobs_cached();
                !jobs.jobs.values().any(|v| v == job) && !jobs.part.values().any(|v| v == job)
            }
        }
    }

    /// Some of a job's work done; told when that earns it the job.
    pub(super) fn work(&mut self, i: usize, job: &str, k: f32) {
        let wk = self.walk_of(i);
        wk.hunger = (wk.hunger + 0.03 * k).min(1.0);
        let name = self.character_name(i);
        match park::add_work(&name, job, k) {
            Some(e) if e.starts_with("part:") => {
                let p = &e["part:".len()..];
                log::info!("{name} now helps out part-time as a {p}");
                tell(&format!("{name} now helps out part-time as a {}", park::job_label(p).to_lowercase()));
            }
            Some(j) => {
                log::info!("{name} is now a {j}");
                super::super::wear::kit_out(&name, &j);
                tell(&format!("{name} is now the {}!", park::job_label(&j).to_lowercase()));
                let wk = self.walk_of(i);
                wk.start_jump(tn().jump_height * 1.2);
            }
            None => {}
        }
    }

    /// A free place to fish at the farm's pond: along the pier first, then the
    /// jetties up the shore. (strip x, height px) Kept for `i` from now on.
    pub(super) fn farm_spot(&mut self, i: usize) -> Option<(f32, f32)> {
        let farm = governor::place_rect("farm").filter(|p| p.ws == self.walk_ref(i).ws)?;
        let size = self.width_of(i);
        let gap = size * 1.05;
        // the pier's places, and two on each jetty (by the shore and at its end)
        let mut pier: Vec<(f32, f32)> = Vec::new();
        let (a, b) = (farm.x + park::PIER.0 * farm.w + size * 0.5, farm.x + park::PIER.1 * farm.w - size * 0.5);
        let mut x = a;
        while x <= b {
            pier.push((x, 0.0));
            x += gap;
        }
        let mut jetties: Vec<(f32, f32)> = Vec::new();
        let (ja, jb) = (farm.x + park::JETTY.0 * farm.w, farm.x + park::JETTY.1 * farm.w);
        for y in park::JETTIES {
            jetties.push((ja + size * 0.35, y * farm.h));
            jetties.push(((jb - size * 0.35).max(ja + size * 0.35 + gap), y * farm.h));
        }
        // spread out: a place on the pier, one up a jetty, the pier again...
        let mut slots: Vec<(f32, f32)> = Vec::new();
        let (mut p, mut j) = (pier.into_iter(), jetties.into_iter());
        loop {
            let (a, b) = (p.next(), j.next());
            if a.is_none() && b.is_none() {
                break;
            }
            slots.extend(a);
            slots.extend(b);
        }
        let mut spots = SPOTS.lock().unwrap();
        spots.retain(|s| s.3.elapsed().as_secs_f32() < 2.0 && s.0 != i);
        // never right above or below another (the jetties climb the shore)
        let free = slots.into_iter().find(|&(x, _)| !spots.iter().any(|s| (s.1 - x).abs() < gap * 0.9))?;
        spots.push((i, free.0, free.1, Instant::now()));
        Some(free)
    }

    /// Still fishing at its place: keeps it.
    pub(super) fn hold_spot(&mut self, i: usize, x: f32, up: f32) {
        let mut spots = SPOTS.lock().unwrap();
        spots.retain(|s| s.0 != i);
        spots.push((i, x, up, Instant::now()));
    }

    /// Off a jetty (or anywhere up the meadow): walks back down to the floor.
    pub(super) fn push_down(&mut self, i: usize) {
        if self.walk_ref(i).air > 0.05 {
            // held up until its walk down starts
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.perched = true;
            }
            DOWN.lock().unwrap().push(i);
        }
    }

    /// Who does `job`'s work now, of those `free`: whoever has the job (even
    /// in another window: it comes over), else someone helping out part-time
    /// (only while the job's own are busy), else anyone without a job (only
    /// while nobody does that job at all). Nearest to `at` first.
    pub(super) fn who_for(&self, job: &str, at: f32, n: usize, free: &dyn Fn(&Self, usize) -> bool) -> Option<usize> {
        let near = |a: &usize, b: &usize| (self.strip_px(*a) - at).abs().total_cmp(&(self.strip_px(*b) - at).abs());
        let full: Vec<usize> = (0..n).filter(|&i| self.is_job(i, job)).collect();
        if let Some(i) = full.iter().copied().filter(|&i| free(self, i)).min_by(near) {
            return Some(i);
        }
        let part: Vec<usize> = (0..n).filter(|&i| self.is_part(i, job)).collect();
        // the job's own are just busy: they'll get to it, unless it's piling up
        if !full.is_empty() && !self.part_needed(job) {
            return None;
        }
        if let Some(i) = part.iter().copied().filter(|&i| free(self, i)).min_by(near) {
            return Some(i);
        }
        if !full.is_empty() || !part.is_empty() {
            return None;
        }
        (0..n).filter(|&i| free(self, i) && park::job_of(&self.character_name(i)).is_none() && park::part_of(&self.character_name(i)).is_none()).min_by(near)
    }

    /// The work `i` should be doing now: its own job, or the one it helps
    /// out with part-time while that's needed.
    pub(in crate::render::character) fn job_now(&self, i: usize) -> Option<String> {
        let name = self.character_name(i);
        if let Some(j) = park::job_of(&name) {
            return Some(j);
        }
        park::part_of(&name).filter(|j| self.part_needed(j))
    }

    /// A part-timer is called in when the job's own aren't there, are on a
    /// break or asleep, or the work is piling up; otherwise it's free.
    pub(in crate::render::character) fn part_needed(&self, job: &str) -> bool {
        let n = self.extras.len() + 1;
        let at = park::JOBS.iter().find(|j| j.0 == job).map_or("", |j| j.2);
        let working = (0..n).filter(|&j| self.is_job(j, job)).any(|j| {
            let wk = self.walk_ref(j);
            self.place_here(j).is_some_and(|p| p.place == at) && !wk.on_break && wk.act != Act::Rest
        });
        if !working {
            return true;
        }
        match job {
            "chef" => dine::ORDERS.lock().unwrap().iter().filter(|o| o.state == dine::Ask::Waiting).count() >= 2,
            "waiter" => dine::ORDERS.lock().unwrap().iter().filter(|o| matches!(o.state, dine::Ask::Asked | dine::Ask::Ready)).count() >= 2,
            "farmer" => {
                let state = park::read_farm_cached();
                park_now().things.iter().filter(|t| t.place == "farm" && park::farm_state(t, &state).1 > 0).count() >= 3
            }
            "fisher" => park::pantry_total_cached() < 4 + tn().staff_keep as u32,
            _ => false,
        }
    }

    /// Work first, at its workplace, with a job there (full or part-time
    /// while needed): on shift, so no chats or games, only the work.
    pub(in crate::render::character) fn on_shift(&self, i: usize) -> bool {
        if !working() {
            return false;
        }
        let Some(job) = self.job_now(i) else { return false };
        let at = park::JOBS.iter().find(|j| j.0 == job).map_or("", |j| j.2);
        self.place_here(i).is_some_and(|p| p.place == at)
    }

    /// In the cafe or on the farm: no games there (the park and the desktop are for that).
    pub(super) fn no_games_here(&self, i: usize) -> bool {
        self.place_here(i).is_some_and(|p| p.place != "park")
    }

    /// The park, cafe or farm window `i` is standing in, if any.
    pub(super) fn place_here(&self, i: usize) -> Option<governor::Place> {
        governor::place_at(self.walk_ref(i).ws, self.strip_px(i))
    }

    fn height_of(&self, i: usize) -> f32 {
        let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        (r[3] - r[1]).max(1.0)
    }

    /// One character height in strip px: what the park's sizes are in.
    fn park_unit(&self, n: usize) -> f32 {
        let sum: f32 = (0..n).map(|i| self.height_of(i)).sum();
        (sum / n.max(1) as f32).max(20.0)
    }

    /// Tells the park window how big a character is, so it draws things to fit.
    pub(super) fn park_tell_unit(&mut self, n: usize) {
        if governor::places().is_empty() || self.view_w < 1.0 {
            return;
        }
        let width = governor::places().first().map_or(self.view_w, |p| p.w.max(1.0));
        let frac = self.park_unit(n) / width;
        let mut c = CACHE.lock().unwrap();
        if (frac - c.unit_written).abs() > c.unit_written * 0.03 {
            c.unit_written = frac;
            let text = format!("{{\"unit_frac\": {frac:.5}}}");
            let _ = crate::util::atomic_write(&park::view_path(), text.as_bytes());
        }
    }

    /// How high up the meadow a thing stands (strip px above the floor).
    fn thing_up(&self, t: &Thing) -> f32 {
        governor::place_rect(&t.place).map_or(0.0, |p| t.y * p.h)
    }

    /// Walks towards `x`, climbing (or coming down) to `up` px above the
    /// floor on the way, as up or down a meadow. True once there.
    pub(super) fn walk_up_to(&mut self, i: usize, x: f32, up: f32, dt: f32) -> bool {
        let here = self.strip_px(i);
        let h = self.height_of(i);
        let surface = self.actor_ref(i).surface[0].max(1.0);
        let now = self.walk_ref(i).air * h * 0.5;
        let dx = x - here;
        let dv = up - now;
        if dx.abs() <= 6.0 && dv.abs() <= 2.0 {
            let wk = self.walk_of(i);
            // on the floor exactly, or held up there: never a sliver of air
            // without a hold (that's a fall, and a fall ends what it's doing)
            let up = if up < 0.5 { 0.0 } else { up };
            wk.air = up / (h * 0.5);
            if let Some(r) = wk.social.as_mut() {
                r.perched = up > 0.0;
                r.moving = false;
            }
            return true;
        }
        let speed = 0.16;
        // across at walking pace (walking as it goes), and up or down in step
        let step = speed * surface * 0.5 * dt;
        let unit = self.park_unit(self.extras.len() + 1);
        let climb = if dx.abs() > 6.0 { (dv.abs() * step / dx.abs()).max(0.3 * unit * dt) } else { 0.8 * unit * dt };
        let next = now + dv.signum() * climb.min(dv.abs());
        if dx.abs() > 6.0 {
            self.go_facing(i, x, speed);
        }
        let wk = self.walk_of(i);
        wk.air = (next / (h * 0.5)).max(0.0);
        if let Some(r) = wk.social.as_mut() {
            // held up the whole way, even the first sliver off the floor
            r.perched = wk.air > 0.0;
        }
        false
    }

    /// To a chair (or off one): down to the floor first, along the floor,
    /// then up onto the seat; never gliding through the air. True once there.
    pub(super) fn seat_walk(&mut self, i: usize, x: f32, up: f32, dt: f32) -> bool {
        let dx = x - self.strip_px(i);
        let h = self.height_of(i);
        let now = self.walk_ref(i).air * h * 0.5;
        if dx.abs() > 6.0 && now > 0.5 {
            // off the seat, straight down
            let unit = self.park_unit(self.extras.len() + 1);
            let next = (now - 1.6 * unit * dt).max(0.0);
            let wk = self.walk_of(i);
            wk.air = next / (h * 0.5);
            if let Some(r) = wk.social.as_mut() {
                r.perched = wk.air > 0.0;
                r.moving = false;
            }
            return false;
        }
        if dx.abs() > 6.0 {
            // along the floor
            if !self.walk_ref(i).social.is_some_and(|s| s.moving) && self.arrived(i) {
                self.go_facing(i, x, 0.15);
            }
            return false;
        }
        // there: up onto it (or down to the floor)
        self.walk_up_to(i, x, up, dt)
    }

    fn thing_px(&self, t: &Thing) -> f32 {
        match column(&t.place) {
            Some((x, w)) => x + t.x * w,
            None => t.x * self.view_w,
        }
    }

    /// Holds `i` at `px` along the floor and `up` px above it.
    fn hold_at(&mut self, i: usize, px: f32, up: f32) {
        let here = self.strip_px(i);
        let h = self.height_of(i);
        let surface = self.actor_ref(i).surface[0].max(1.0);
        let wk = self.walk_of(i);
        wk.x += (px - here) * 2.0 / surface;
        wk.target = wk.x;
        wk.air = (up / (h * 0.5)).max(0.0);
        if matches!(wk.act, Act::Walk) {
            wk.act = Act::Idle;
        }
        if let Some(r) = wk.social.as_mut() {
            r.perched = true;
            r.moving = false;
            r.goal = wk.x;
            r.goal_px = px;
        }
    }

    /// Back on the ground and free to walk.
    fn let_down(&mut self, i: usize) {
        let wk = self.walk_of(i);
        wk.air = 0.0;
        wk.sink = 0.0;
        if let Some(r) = wk.social.as_mut() {
            r.perched = false;
            r.happy = false;
        }
    }

    fn set_happy(&mut self, i: usize, on: bool) {
        if let Some(r) = self.walk_of(i).social.as_mut() {
            r.happy = on;
        }
    }

    fn set_face(&mut self, i: usize, left: bool) {
        if let Some(r) = self.walk_of(i).social.as_mut() {
            r.face_left = left;
        }
    }

    /// Once in a while someone free in the park goes to play on something
    /// there, or joins whoever is on it already.
    pub(super) fn park_play(&mut self, chats: &mut Vec<Chat>, n: usize) {
        let homes = governor::places();
        if homes.is_empty() {
            return;
        }
        let who: Vec<usize> = (0..n)
            .filter(|&i| {
                homes.iter().any(|h| h.ws == self.walk_ref(i).ws) && self.free_for(i, Want::Play)
            })
            .collect();
        if who.is_empty() {
            return;
        }
        let i = who[(self.walk.rand() * who.len() as f32) as usize % who.len()];
        self.play_for(chats, i, n, false);
    }

    /// Something for `i` to do where it is: a stroll up the meadow, a hen to
    /// chase, a seat to eat at, a go on something. `force`: no dice and no cap
    /// on how many are busy (it's been standing about). True if it got one.
    fn play_for(&mut self, chats: &mut Vec<Chat>, i: usize, n: usize, force: bool) -> bool {
        let park = park_now();
        let Some(place) = self.place_here(i).map(|p| p.place) else { return false };
        let busy = !force && self.too_busy(chats, n);
        // on the farm: now and then a stroll up the meadow
        if place == "farm" && self.walk_of(i).rand() < tn().meadow_stroll && !busy {
            self.start_play(chats, i, usize::MAX, "meadow");
            return true;
        }
        // on the farm: now and then after a hen instead
        if place == "farm" && (force || self.walk_of(i).rand() < tn().chase_chicken) && !busy {
            let hens: Vec<usize> = LIVE.lock().unwrap().beasts.iter().enumerate().filter(|b| b.1.kind == "chicken" && b.1.flee_t <= 0.0).map(|b| b.0).collect();
            let taken: Vec<usize> = chats.iter().filter_map(|c| c.play.as_ref()).filter(|p| p.kind == "chase").map(|p| p.thing).collect();
            if let Some(&k) = hens.iter().find(|k| !taken.contains(k)) {
                self.start_play(chats, i, k, "chase");
                return true;
            }
        }
        // hungry in the cafe, the chef in: a seat at a table
        if place == "cafe" && !playing_only() && self.maybe_dine(chats, i) {
            return true;
        }
        if !force && (self.walk.rand() >= tn().park_play || busy) {
            return false;
        }
        let restaurant = self.restaurant_open();
        // something with room; somewhere a friend is waiting counts for more
        let job = park::job_of(&self.character_name(i));
        let chef_here = (0..n).any(|j| j != i && self.is_job(j, "chef") && self.place_here(j).is_some_and(|p| p.place == place));
        let mut options: Vec<(usize, f32)> = Vec::new();
        for (k, t) in park.things.iter().enumerate() {
            let room = park::room(&t.kind);
            // tables are for eating; the stove is the chef's once there is one
            // the stove is for cooking a meal: only the hungry (or the chef) go
            let cook_ok = self.hungry(i) || job.as_deref() == Some("chef");
            if room == 0 || t.place != place || t.kind == "table" || (t.kind == "chair" && t.place == "cafe") || (restaurant && t.kind == "stove" && t.place == "cafe") || (t.kind == "stove" && !cook_ok) {
                continue;
            }
            // farm work is the farmer's (if there is one); others with a job keep to theirs
            // (on their own only the farmers go; the others are called when needed)
            let farm_work = matches!(t.kind.as_str(), "carrots" | "wheat" | "apple_tree" | "coop" | "barn") || (t.kind == "tree" && t.place == "farm");
            if farm_work && !self.is_job(i, "farmer") {
                continue;
            }
            // only playing: no work
            if playing_only() && work_thing(&t.kind) {
                continue;
            }
            let on = chats.iter().find(|c| c.stage == Stage::Play && c.play.as_ref().is_some_and(|p| p.thing == k && p.kind == t.kind));
            let used = on.map_or(0, |c| c.members.len());
            if used >= room {
                continue;
            }
            let waiting = matches!(t.kind.as_str(), "seesaw" | "table" | "bench") && used == 1;
            let mut weight = if waiting { 3.0 } else { 1.0 };
            // what its job is about draws it more; the chef's stove is the chef's
            weight *= match (job.as_deref(), t.kind.as_str()) {
                (Some("attendant"), "swing") if used == 1 => 6.0,
                (Some("attendant"), "slide" | "swing" | "seesaw" | "sandbox") => 4.0,
                (Some("chef"), "stove") => 6.0,
                (Some("farmer"), "carrots" | "wheat" | "apple_tree" | "coop" | "barn") => 4.0,
                (Some("waiter"), "table") => 2.0,
                (j, "stove") if j != Some("chef") && chef_here => 0.3,
                _ => 1.0,
            };
            // eggs, milk or a crop waiting: hard to walk past
            if park::farm_state(t, &park::read_farm_cached()).1 > 0 {
                weight *= 8.0;
            }
            // work first: the fun things less often
            if working() && fun_thing(&t.kind) {
                weight *= 0.3;
            }
            options.push((k, weight));
        }
        let total: f32 = options.iter().map(|o| o.1).sum();
        if total <= 0.0 {
            return false;
        }
        let mut roll = self.walk.rand() * total;
        let Some(&(k, _)) = options.iter().find(|o| {
            roll -= o.1;
            roll <= 0.0
        }) else {
            return false;
        };
        let kind = park.things[k].kind.clone();
        self.start_play(chats, i, k, &kind);
        true
    }

    /// Enough of them off doing things on their own (on the slide, up the
    /// meadow, at the coop...): the rest stay free to get together and play.
    fn too_busy(&self, chats: &[Chat], n: usize) -> bool {
        let busy: usize = chats
            .iter()
            .filter_map(|c| c.play.as_ref().map(|p| (p, c.members.len())))
            .filter(|(p, _)| !matches!(p.kind.as_str(), "dine" | "order" | "serve" | "down"))
            .map(|(_, k)| k)
            .sum();
        busy as f32 >= n as f32 * tn().park_busy_most
    }

    /// Game time in the park: now and then the free ones in it (and nearby)
    /// get together for one of the usual games.
    fn game_time(&mut self, chats: &mut Vec<Chat>, n: usize) {
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        // volunteers on their way from the desktop: the game once they're there
        static GATHER: Mutex<Option<Instant>> = Mutex::new(None);
        let gather_now = {
            let mut g = GATHER.lock().unwrap();
            if g.is_some_and(|t| Instant::now() >= t) {
                *g = None;
                true
            } else {
                false
            }
        };
        if !gather_now {
            let mut next = NEXT.lock().unwrap();
            let every = tn().park_game_every.max(10.0);
            match *next {
                None => {
                    *next = Some(Instant::now() + std::time::Duration::from_secs_f32(every * 0.5));
                    return;
                }
                Some(t) if Instant::now() < t => return,
                _ => {}
            }
            let r = self.walk.rand();
            // work first: game time a third as often; with nobody to call it, rarer still
            let every = if working() { every * 3.0 } else { every };
            let every = if self.park_host(n).is_none() { every * 1.5 } else { every };
            *next = Some(Instant::now() + std::time::Duration::from_secs_f32(every * (0.6 + r * 0.8)));
            drop(next);
            if visit::visiting_mode() {
                if self.game_volunteers(n) > 0 {
                    *GATHER.lock().unwrap() = Some(Instant::now() + std::time::Duration::from_secs(25));
                }
                return;
            }
        }
        let Some(park) = governor::place_rect("park") else { return };
        if self.race.is_some() {
            return;
        }
        let middle = park.x + park.w * 0.5;
        let host = self.park_host(n);
        // the park's free ones first, then the nearest from next door
        let mut free: Vec<(usize, f32)> = (0..n)
            .filter(|&i| {
                let wk = self.walk_ref(i);
                Some(i) != host && wk.ws == park.ws && wk.at_leisure() && wk.trip.is_none() && wk.rest.is_none() && !self.on_shift(i) && wk.energy >= tn().energy_game_need * 0.5
            })
            .map(|i| (i, (self.strip_px(i) - middle).abs()))
            .collect();
        free.sort_by(|a, b| a.1.total_cmp(&b.1));
        free.truncate(6);
        if free.len() < 3 {
            log::debug!("park: game time, but only {} free", free.len());
            return;
        }
        let members: Vec<usize> = free.iter().map(|f| f.0).collect();
        let rivalry = self.rivalry(&members);
        let Some(game) = self.choose_game(&[], members.len(), rivalry) else { return };
        self.chat_id = self.chat_id.wrapping_add(1);
        let id = self.chat_id;
        for &i in &members {
            self.enlist(i, id);
        }
        log::debug!("chat {id}: game time in the park, {game:?} with {members:?}");
        let mut c = Chat::new(id, members, Stage::Approach, 0.0);
        c.forced = Some(game);
        self.arrange(&c);
        if let Some(h) = host {
            self.host_call(&mut c, h, middle);
        }
        chats.push(c);
    }

    /// `i` goes to play on thing `k`, with whoever is on it already.
    fn start_play(&mut self, chats: &mut Vec<Chat>, i: usize, k: usize, kind: &str) {
        let kind = kind.to_string();
        let own = matches!(kind.as_str(), "serve" | "order" | "down" | "meadow" | "chase");
        if let Some(c) = chats.iter_mut().filter(|_| !own).find(|c| c.stage == Stage::Play && c.play.as_ref().is_some_and(|p| p.thing == k && p.kind == kind)) {
            self.enlist(i, c.id);
            c.members.push(i);
            log::debug!("chat {}: {i} joins in on the {kind}", c.id);
            return;
        }
        self.chat_id = self.chat_id.wrapping_add(1);
        let id = self.chat_id;
        self.enlist(i, id);
        let mut c = Chat::new(id, vec![i], Stage::Play, 600.0);
        let stay = match kind.as_str() {
            "table" => 25.0 + self.walk.rand() * 20.0,
            "chair" | "bench" => 12.0 + self.walk.rand() * 20.0,
            "stove" => 9.0 + self.walk.rand() * 5.0,
            "sandbox" => 15.0 + self.walk.rand() * 15.0,
            "seesaw" => 10.0 + self.walk.rand() * 8.0,
            _ => 0.0,
        };
        c.play = Some(Play { thing: k, kind: kind.clone(), riders: Vec::new(), t: 0.0, stay, meal: None, from: 0.0, counter: 0, by_cook: false, diner: usize::MAX, shop: false });
        log::debug!("chat {id}: {i} goes to the {kind}");
        chats.push(c);
    }

    pub(super) fn run_play(&mut self, c: &mut Chat, dt: f32) -> bool {
        let Some(mut p) = c.play.take() else { return false };
        let park = park_now();
        let thing = park.things.get(p.thing).filter(|t| t.kind == p.kind).cloned();
        if p.kind == "picnic" {
            let done = self.play_picnic(c, &mut p, dt);
            if done {
                for &i in &c.members.clone() {
                    self.let_down(i);
                }
                return false;
            }
            c.play = Some(p);
            return !c.members.is_empty();
        }
        if p.kind == "meadow" {
            let done = self.play_meadow(c, &mut p, dt);
            if done {
                self.let_down(c.members[0]);
                return false;
            }
            c.play = Some(p);
            return true;
        }
        if p.kind == "down" {
            let i = c.members[0];
            let unit = self.park_unit(self.extras.len() + 1);
            if p.riders.is_empty() {
                let side = if self.walk.rand() < 0.5 { -1.0 } else { 1.0 };
                p.from = self.strip_px(i) + side * 0.7 * unit;
                p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 0 });
            }
            p.t += dt;
            let x = p.from;
            if self.walk_up_to(i, x, 0.0, dt) || p.t > 20.0 {
                self.let_down(i);
                return false;
            }
            c.play = Some(p);
            return true;
        }
        if p.kind == "order" {
            let done = self.play_order(c, &mut p, dt);
            if done {
                return false;
            }
            c.play = Some(p);
            return !c.members.is_empty();
        }
        if p.kind == "dine" {
            let table = park.things.get(p.thing).filter(|t| t.kind == "table").cloned();
            if let Some(t) = table {
                p.riders.retain(|r| c.members.contains(&r.i));
                for &i in &c.members {
                    if !p.riders.iter().any(|r| r.i == i) {
                        p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 0 });
                    }
                }
                p.t += dt;
                let unit = self.park_unit(self.extras.len() + 1);
                let mid = self.thing_px(&t);
                if !self.play_dine(c, &mut p, &t, mid, unit, dt) {
                    c.play = Some(p);
                    return !c.members.is_empty();
                }
            }
            for &i in &c.members.clone() {
                self.let_down(i);
            }
            return false;
        }
        if p.kind == "serve" {
            if p.riders.is_empty() {
                p.riders.push(Rider { i: c.members[0], phase: 0, t: 0.0, n: 0, want: 0 });
            }
            let table = park.things.get(p.thing).filter(|t| t.kind == "table").cloned();
            p.t += dt;
            let done = match table {
                Some(t) => self.play_serve(c, &mut p, &t, dt),
                None => true,
            };
            if done {
                for &i in &c.members.clone() {
                    self.let_down(i);
                }
                return false;
            }
            c.play = Some(p);
            return !c.members.is_empty();
        }
        if p.kind == "chase" {
            let done = self.play_chase(c, &mut p, dt);
            if done {
                for &i in &c.members.clone() {
                    self.let_down(i);
                }
                return false;
            }
            c.play = Some(p);
            return !c.members.is_empty();
        }
        let Some(thing) = thing.filter(|t| governor::place_workspace(&t.place).is_some()) else {
            // taken down, or the park closed: off they get
            for &i in &c.members.clone() {
                self.let_down(i);
            }
            return false;
        };
        p.riders.retain(|r| c.members.contains(&r.i));
        for &i in &c.members {
            if !p.riders.iter().any(|r| r.i == i) {
                let want = 2 + (self.walk.rand() * 3.0) as u8;
                p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want });
            }
        }
        if p.riders.is_empty() {
            return false;
        }
        p.t += dt;
        let unit = self.park_unit(self.extras.len() + 1);
        let mid = self.thing_px(&thing);
        let mut done = match thing.kind.as_str() {
            "slide" => self.play_slide(c, &mut p, &thing, mid, unit, dt),
            "swing" => self.play_swing(c, &mut p, mid, unit, dt),
            "seesaw" => self.play_seesaw(c, &mut p, mid, unit, dt),
            "chair" | "bench" => self.play_sit(c, &mut p, &thing, mid, unit),
            "table" => self.play_table(c, &mut p, &thing, mid, unit, dt),
            "stove" => self.play_stove(c, &mut p, &thing, mid, unit, dt),
            "carrots" | "wheat" | "apple_tree" | "coop" | "barn" | "tree" => self.play_farm(c, &mut p, &thing, mid, unit, dt),
            "sandbox" => self.play_sandbox(c, &mut p, mid, unit, dt),
            "stall" if p.shop => self.play_shop(c, &mut p, mid, unit, dt),
            "stall" => self.play_stall(c, &mut p, mid, unit, dt),
            _ => true,
        };
        if done && thing.place == "park" && matches!(thing.kind.as_str(), "slide" | "swing" | "seesaw" | "sandbox") {
            for &i in &c.members.clone() {
                self.work(i, "attendant", 1.0);
                // only playing: it tires them, and tired out they nap it off
                if playing_only() {
                    self.spend(i, tn().play_tire);
                    self.walk_of(i).played += 0.5;
                }
            }
        }
        if done && matches!(thing.kind.as_str(), "table") {
            for &i in &c.members.clone() {
                self.push_down(i);
            }
        }
        if done && thing.kind == "stove" {
            if let Some(k) = p.meal.as_ref().and_then(|m| m.dish.strip_prefix("table:")).and_then(|k| k.parse::<usize>().ok()) {
                let server = SERVE.lock().unwrap().iter().find(|s| s.table == k).map(|s| s.who);
                let mut q = PENDING.lock().unwrap();
                // the cook, and those waiting with it who are hungry too
                let cook = p.riders.first().map(|r| r.i);
                for r in p.riders.iter().filter(|r| Some(r.i) != server && (Some(r.i) == cook || self.hungry(r.i))) {
                    q.push((r.i, k));
                }
            }
        }
        if done {
            for &i in &c.members.clone() {
                self.let_down(i);
                self.leave(i, c.id);
            }
            c.members.clear();
            done = true;
        }
        if done {
            return false;
        }
        c.play = Some(p);
        !c.members.is_empty()
    }

    /// Leaves the play (and the chat) on its own, the others carry on.
    fn get_off(&mut self, c: &mut Chat, p: &mut Play, i: usize) {
        self.let_down(i);
        p.riders.retain(|r| r.i != i);
        c.members.retain(|&m| m != i);
        self.leave(i, c.id);
    }

    /// Up the ladder one at a time, a moment at the top, down the chute,
    /// a happy hop at the bottom and round again.
    fn play_slide(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, mid: f32, unit: f32, dt: f32) -> bool {
        let (top, reach) = park::slide_size(t.pieces);
        let (top, reach) = (top * unit, reach * unit);
        let dir = if t.flip { -1.0 } else { 1.0 };
        let half = park::width(t) * unit * 0.5;
        let ladder = mid - dir * (half - 0.12 * unit);
        let lip = ladder + dir * 0.25 * unit;
        // someone already on the ladder or at the top
        let busy = p.riders.iter().any(|r| matches!(r.phase, 1 | 2));
        let mut off = Vec::new();
        let mut queue = 0.0;
        for k in 0..p.riders.len() {
            let mut r = p.riders[k];
            r.t += dt;
            match r.phase {
                0 => {
                    let spot = ladder - dir * (0.35 + queue * 0.45) * unit;
                    queue += 1.0;
                    if !self.walk_ref(r.i).social.is_some_and(|s| s.moving) && (self.strip_px(r.i) - spot).abs() > 8.0 && self.arrived(r.i) {
                        self.go(r.i, spot, 0.16);
                    }
                    if queue <= 1.0 && !busy && self.arrived(r.i) && (self.strip_px(r.i) - spot).abs() <= 8.0 {
                        r.phase = 1;
                        r.t = 0.0;
                    }
                }
                1 => {
                    let climb = 0.6 + top / unit * 0.9;
                    let u = (r.t / climb).min(1.0);
                    // smoothly up, easing in and out
                    let up = u * u * (3.0 - 2.0 * u);
                    self.set_face(r.i, dir < 0.0);
                    self.hold_at(r.i, ladder, top * up);
                    if u >= 1.0 {
                        r.phase = 2;
                        r.t = 0.0;
                    }
                }
                2 => {
                    let u = (r.t / 0.5).min(1.0);
                    self.hold_at(r.i, ladder + (lip - ladder) * u, top);
                    if r.t > 0.7 {
                        self.set_happy(r.i, true);
                        r.phase = 3;
                        r.t = 0.0;
                    }
                }
                3 => {
                    let dur = 0.45 + 0.22 * reach / unit;
                    let u = (r.t / dur).min(1.0);
                    // faster and faster, levelling out at the end
                    let s = u * u * (1.6 - 0.6 * u);
                    let x = lip + dir * reach * s;
                    let y = top * (1.0 - s).powf(1.15);
                    self.hold_at(r.i, x, y);
                    if u >= 1.0 {
                        r.phase = 4;
                        r.t = 0.0;
                        r.n += 1;
                        self.let_down(r.i);
                        let wk = self.walk_of(r.i);
                        wk.start_jump(tn().jump_height * 0.6);
                        let secs = tn().laugh_time * 0.8;
                        wk.laugh(secs);
                    }
                }
                _ => {
                    if r.t > 1.0 && self.arrived(r.i) {
                        if r.n >= r.want {
                            off.push(r.i);
                        } else {
                            r.phase = 0;
                            r.t = 0.0;
                        }
                    }
                }
            }
            p.riders[k] = r;
        }
        for i in off {
            self.get_off(c, p, i);
        }
        c.members.is_empty()
    }

    /// Back and forth, higher and higher; a friend gives a push from behind.
    /// Then they swap.
    fn play_swing(&mut self, _c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
        let top = park::SWING_TOP * unit;
        let rope = park::SWING_ROPE * unit;
        let rider = p.riders[0].i;
        let pusher = p.riders.get(1).map(|r| r.i);
        let mut r = p.riders[0];
        r.t += dt;
        match r.phase {
            0 => {
                if !self.walk_ref(rider).social.is_some_and(|s| s.moving) && (self.strip_px(rider) - mid).abs() > 6.0 && self.arrived(rider) {
                    self.go(rider, mid, 0.16);
                } else if self.arrived(rider) && (self.strip_px(rider) - mid).abs() <= 6.0 {
                    r.phase = 1;
                    r.t = 0.0;
                    r.want = 12 + (self.walk.rand() * 8.0) as u8;
                }
            }
            1 => {
                let len = r.want as f32;
                let most = if pusher.is_some() { 0.85 } else { 0.6 };
                let grow = (r.t / 4.0).min(1.0);
                let fade = ((len - r.t) / 3.0).clamp(0.0, 1.0);
                let amp = most * grow * fade;
                let a = amp * (r.t * std::f32::consts::TAU / 2.4).sin();
                self.set_face(rider, false);
                self.set_happy(rider, amp > 0.2);
                self.hold_at(rider, mid + rope * a.sin(), top - rope * a.cos());
                if let Some(q) = pusher {
                    let behind = mid - 0.95 * unit;
                    if (self.strip_px(q) - behind).abs() > 6.0 && self.arrived(q) && !self.walk_ref(q).social.is_some_and(|s| s.moving) {
                        self.go(q, behind, 0.16);
                    } else if self.arrived(q) {
                        self.set_face(q, false);
                        // pushes as it comes back
                        let back = (r.t * std::f32::consts::TAU / 2.4).sin() < -0.95 * grow;
                        if back && self.walk.rand() < dt * 6.0 {
                            self.walk_of(q).gesture = true;
                            self.walk_of(q).start_bob(tn().jump_height * 0.1);
                        }
                    }
                }
                if r.t >= len {
                    self.let_down(rider);
                    let wk = self.walk_of(rider);
                    wk.start_jump(tn().jump_height * 0.5);
                    let secs = tn().laugh_time * 0.6;
                    wk.laugh(secs);
                    r.phase = 2;
                    r.t = 0.0;
                }
            }
            _ => {
                if r.t > 1.2 {
                    p.riders[0] = r;
                    // the friend's turn, once
                    if let Some(q) = pusher.filter(|_| r.n == 0) {
                        p.riders.swap(0, 1);
                        p.riders[0] = Rider { i: q, phase: 0, t: 0.0, n: 1, want: 0 };
                        p.riders[1].n = 1;
                        return false;
                    }
                    return true;
                }
            }
        }
        p.riders[0] = r;
        false
    }

    /// One at each end, up and down; alone it waits a while for someone.
    fn play_seesaw(&mut self, c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
        let half = park::SEESAW_HALF * unit * 0.92;
        let pivot = park::SEESAW_PIVOT * unit;
        let tilt = park::SEESAW_TILT * unit;
        let ends = [mid - half, mid + half];
        let mut all_on = true;
        for k in 0..p.riders.len().min(2) {
            let i = p.riders[k].i;
            let end = ends[k];
            if p.riders[k].phase == 0 {
                if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - end).abs() > 6.0 && self.arrived(i) {
                    self.go(i, end, 0.16);
                } else if self.arrived(i) && (self.strip_px(i) - end).abs() <= 6.0 {
                    p.riders[k].phase = 1;
                }
                all_on = false;
            }
        }
        let pair = p.riders.len() >= 2 && all_on;
        if !pair {
            // waiting on its end, board down on its side
            for k in 0..p.riders.len().min(2) {
                if p.riders[k].phase == 1 {
                    let i = p.riders[k].i;
                    self.set_face(i, k == 1);
                    self.hold_at(i, ends[k], pivot - tilt);
                }
            }
            if p.t > 12.0 && p.riders.len() < 2 {
                return true;
            }
            return false;
        }
        p.riders[0].t += dt;
        let t = p.riders[0].t;
        let s = (t * std::f32::consts::TAU / 1.8).sin();
        let heights = [pivot - tilt * s, pivot + tilt * s];
        for k in 0..2 {
            let i = p.riders[k].i;
            self.set_face(i, k == 1);
            self.set_happy(i, true);
            self.hold_at(i, ends[k], heights[k].max(0.0));
        }
        let _ = c;
        t > p.stay
    }

    /// Sits down on the seat for a while (two on a bench, turning to talk).
    fn play_sit(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, mid: f32, unit: f32) -> bool {
        let seats: Vec<f32> = if t.kind == "bench" { vec![mid - 0.25 * unit, mid + 0.25 * unit] } else { vec![mid] };
        let seat = park::SEAT * unit;
        for k in 0..p.riders.len().min(seats.len()) {
            let i = p.riders[k].i;
            let spot = seats[k];
            if p.riders[k].phase == 0 {
                if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - spot).abs() > 6.0 && self.arrived(i) {
                    self.go(i, spot, 0.14);
                } else if self.arrived(i) && (self.strip_px(i) - spot).abs() <= 6.0 {
                    p.riders[k].phase = 1;
                    // a little hop up onto it
                    self.walk_of(i).start_bob(tn().jump_height * 0.15);
                }
                continue;
            }
            // on a chair: up on the seat (seen from the side); on a bench the
            // legs hang down in front of the plank
            let bench = t.kind == "bench";
            self.hold_at(i, spot, if bench { seat * 0.75 } else { seat + 0.02 * unit });
            let other = p.riders.iter().map(|r| r.i).find(|&o| o != i);
            let wk = self.walk_of(i);
            wk.sink = if bench { tn().sit_sink } else { 0.0 };
            if let Some(o) = other {
                if wk.rand() < 0.004 {
                    wk.gesture = true;
                }
                self.face(i, o);
            }
        }
        let _ = c;
        p.t > p.stay
    }

    /// Round the table, chatting; alone it waits a while for company. With
    /// a dish on it, they eat it together and get closer.
    fn play_table(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, mid: f32, unit: f32, dt: f32) -> bool {
        let seats = self.table_seats(t, mid, unit);
        // no place of its own at this table (diners may have it): it doesn't stay
        for i in p.riders.iter().map(|r| r.i).collect::<Vec<_>>() {
            if dine::seat_slot(p.thing, i, seats.len()).is_none() {
                self.get_off(c, p, i);
            }
        }
        for k in 0..p.riders.len() {
            let i = p.riders[k].i;
            let Some((x, up)) = dine::seat_slot(p.thing, i, seats.len()).map(|s| seats[s]) else { continue };
            if p.riders[k].phase == 0 {
                if self.seat_walk(i, x, up, dt) {
                    p.riders[k].phase = 1;
                    self.set_face(i, x > mid);
                }
            } else if up > 0.0 {
                self.seat_walk(i, x, up, dt);
            }
        }
        if let Some((dish, left)) = dish_on(t.id) {
            let eaters: Vec<usize> = p.riders.iter().filter(|r| r.phase == 1).map(|r| r.i).collect();
            if !eaters.is_empty() {
                // a bite now and then, happy about it
                for &i in &eaters {
                    if self.walk.rand() < dt * 0.9 {
                        self.walk_of(i).start_bob(tn().jump_height * 0.07);
                    }
                    let glad = dish != "burnt" || self.walk.rand() < 0.5;
                    self.set_happy(i, glad);
                }
                let left = left - dt * 0.018 * eaters.len() as f32;
                let mut l = LIVE.lock().unwrap();
                if left <= 0.0 {
                    l.dishes.retain(|d| d.0 != t.id);
                    drop(l);
                    log::debug!("chat {}: the {dish} is all eaten", c.id);
                    for (a, &x) in eaters.iter().enumerate() {
                        self.eat(x, &dish);
                        for &y in &eaters[a + 1..] {
                            self.add_bond(x, y, tn().bond_game * 1.5, 0.0);
                        }
                        let wk = self.walk_of(x);
                        wk.start_jump(tn().jump_height * 0.6);
                    }
                    p.stay = p.t + 3.0;
                } else if let Some(d) = l.dishes.iter_mut().find(|d| d.0 == t.id) {
                    d.2 = left;
                }
                return false;
            }
            // food waiting: nobody leaves before it's eaten
            p.stay = p.stay.max(p.t + 5.0);
        }
        if c.members.len() >= 2 {
            self.chatter(c, dt);
        } else if p.t > 15.0 && dish_on(t.id).is_none() {
            return true;
        }
        p.t > p.stay
    }

    /// At the stove: the cook takes what's in the pantry and cooks, a recipe
    /// it knows or something new; others come and wait. Little hops while it
    /// cooks; a new recipe found, a happy jump; burnt, a hop back and the
    /// others laugh. What's made goes on the nearest table to be eaten.
    fn play_stove(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, mid: f32, unit: f32, dt: f32) -> bool {
        for k in 0..p.riders.len() {
            let i = p.riders[k].i;
            let spot = if k == 0 { mid } else { mid + (if k % 2 == 1 { 1.0 } else { -1.0 }) * (0.8 + 0.4 * (k / 2) as f32) * unit };
            if p.riders[k].phase == 0 {
                if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - spot).abs() > 6.0 && self.arrived(i) {
                    self.go(i, spot, 0.14);
                } else if self.arrived(i) && (self.strip_px(i) - spot).abs() <= 6.0 {
                    p.riders[k].phase = 1;
                }
                continue;
            }
            if k > 0 {
                self.face(i, p.riders[0].i);
            }
        }
        let cook = p.riders[0].i;
        if p.riders[0].phase != 1 {
            return false;
        }
        // decide what to make, once there
        if p.meal.is_none() {
            let choice = self.choose_meal(cook);
            if choice.is_none() && p.diner == dine::PREP {
                return true;
            }
            if choice.is_none() && p.diner != usize::MAX {
                // nothing to cook it with
                self.order_failed(p.diner);
                return true;
            }
            match choice {
                Some(m) => {
                    log::debug!("chat {}: {cook} cooks {:?} -> {}", c.id, m.stuff, m.dish);
                    p.meal = Some(m);
                    p.stay = 8.0 + self.walk.rand() * 6.0;
                    p.t = 0.0;
                }
                // nothing in the pantry: a look round and off
                None => return p.t > 4.0,
            }
        }
        let Some(m) = p.meal.clone() else { return true };
        if p.t < p.stay {
            set_stove(t.id, "cooking", p.t / p.stay);
            if self.walk.rand() < dt * 0.8 {
                self.walk_of(cook).start_bob(tn().jump_height * 0.08);
            }
            // the others just wait (no hopping about: it looked like a fault)
            return false;
        }
        if p.riders[0].phase == 1 && p.riders[0].n == 0 {
            p.riders[0].n = 1;
            self.practice(&[cook], "cooking");
            self.work(cook, "chef", if m.new { 3.0 } else { 2.0 });
            let name = self.character_name(cook);
            let mut book = park::read_book();
            if m.burnt {
                set_stove(t.id, "burnt", 1.0);
                if !m.new && park::dish_of(m.stuff.iter().map(|x| x.as_str()).collect()).is_none() {
                    let mut stuff = m.stuff.clone();
                    stuff.sort();
                    if !book.failed.contains(&stuff) {
                        book.failed.push(stuff);
                    }
                }
                park::write_book(&book);
                let wk = self.walk_of(cook);
                wk.start_jump(tn().jump_height * 0.35);
                for r in p.riders.clone().iter().skip(1) {
                    self.set_happy(r.i, true);
                    let wk = self.walk_of(r.i);
                    wk.start_jump(tn().jump_height * 0.4);
                    let secs = tn().laugh_time * 0.6;
                    wk.laugh(secs);
                }
            } else {
                set_stove(t.id, "done", 1.0);
                let e = book.found.entry(m.dish.clone()).or_insert(park::Found { by: name.clone(), times: 0 });
                e.times += 1;
                park::write_book(&book);
                if m.new {
                    tell(&format!("{name} found a new recipe: {}!", park::dish_label(&m.dish)));
                }
                self.set_happy(cook, true);
                let wk = self.walk_of(cook);
                wk.start_jump(tn().jump_height * if m.new { 1.2 } else { 0.7 });
                let secs = tn().laugh_time;
                wk.laugh(secs);
            }
            // cooked for the counter: onto it, for whoever's hungry
            if p.diner == dine::PREP {
                if !m.burnt {
                    if let Some((_, cid)) = self.pickup().filter(|x| x.1 != 0) {
                        LIVE.lock().unwrap().dishes.push((cid, m.dish.clone(), 1.0));
                    }
                }
                p.t = p.stay;
                p.meal = Some(Meal { dish: String::new(), ..m });
                return false;
            }
            // a diner's order: to them, or burnt and made again
            if p.diner != usize::MAX {
                let dish = (!m.burnt).then(|| m.dish.clone());
                dine::set_order(p.diner, |o| o.cook = Some(cook));
                self.order_cooked(p.diner, dish, mid);
                p.t = p.stay;
                p.meal = Some(Meal { dish: String::new(), ..m });
                return false;
            }
            // now and then it's eaten as a picnic in the park
            if !m.burnt && self.walk.rand() < tn().picnic_chance {
                if let Some(pk) = governor::place_rect("park").filter(|pk| pk.ws == self.walk_ref(cook).ws) {
                    let r = self.walk.rand();
                    let x = pk.x + pk.w * (0.2 + 0.6 * r);
                    let mut q = hunger::PICNIC.lock().unwrap();
                    for (n, rd) in p.riders.iter().enumerate() {
                        if n > 0 && !self.hungry(rd.i) {
                            continue;
                        }
                        q.push((rd.i, x, m.dish.clone(), Instant::now()));
                    }
                    log::debug!("chat {}: the {} goes out for a picnic", c.id, m.dish);
                    p.t = p.stay;
                    p.meal = Some(Meal { dish: String::new(), ..m });
                    return false;
                }
            }
            // onto the nearest table in this room, if there's a free one
            let park = park_now();
            let table = park
                .things
                .iter()
                .enumerate()
                .filter(|(_, x)| x.kind == "table" && x.place == t.place && dish_on(x.id).is_none())
                .min_by(|a, b| (a.1.x - t.x).abs().total_cmp(&(b.1.x - t.x).abs()))
                .map(|(k, x)| (k, x.id));
            if m.burnt || table.is_none() {
                // burnt: a brave bite each; no table free: eaten standing round the stove
                for r in p.riders.clone() {
                    self.eat(r.i, &m.dish);
                }
            }
            if let Some((k, id)) = table.filter(|_| !m.burnt) {
                // carried over by a waiter in the room, or one of those waiting;
                // with nobody to carry it, the cook puts it down there itself
                let ws = self.walk_ref(cook).ws;
                let waiter = (0..self.extras.len() + 1).find(|&j| self.is_job(j, "waiter") && self.walk_ref(j).ws == ws && self.place_here(j).is_some_and(|q| q.place == t.place) && (self.walk_ref(j).at_leisure() || p.riders.iter().any(|r| r.i == j)));
                let helper = waiter.or_else(|| p.riders.iter().skip(1).map(|r| r.i).next());
                // a serving counter in the room: the dish goes there first
                let counter = park
                    .things
                    .iter()
                    .filter(|x| x.kind == "counter" && x.place == t.place)
                    .min_by(|a, b| (a.x - t.x).abs().total_cmp(&(b.x - t.x).abs()))
                    .map(|x| (x.id, self.thing_px(x)));
                match (helper, counter) {
                    (h, Some((cid, cx))) => {
                        LIVE.lock().unwrap().dishes.push((cid, m.dish.clone(), 1.0));
                        SERVE.lock().unwrap().push(Serve { who: h.unwrap_or(cook), table: k, dish: m.dish.clone(), from: cx, counter: cid, by_cook: h.is_none(), diner: None });
                    }
                    (Some(h), None) => SERVE.lock().unwrap().push(Serve { who: h, table: k, dish: m.dish.clone(), from: mid, counter: 0, by_cook: false, diner: None }),
                    (None, None) => LIVE.lock().unwrap().dishes.push((id, m.dish.clone(), 1.0)),
                }
                p.meal = Some(Meal { dish: format!("table:{k}"), ..m });
            }
            p.t = p.stay;
        }
        // a moment to see it, then off to eat
        if p.t > p.stay + 2.5 {
            set_stove(t.id, "", 0.0);
            return true;
        }
        false
    }

    /// What the cook makes: a known recipe it has the things for, or a new
    /// mix (never one that went wrong before). None with too little in the pantry.
    fn choose_meal(&mut self, cook: usize) -> Option<Meal> {
        let pantry = park::read_pantry();
        let have: Vec<&str> = park::INGREDIENTS.iter().map(|x| x.0).filter(|x| pantry.get(*x).copied().unwrap_or(0) > 0).collect();
        if have.len() < 2 {
            return None;
        }
        let book = park::read_book();
        let known: Vec<&(&str, &str, &[&str])> = park::DISHES.iter().filter(|d| book.found.contains_key(d.0) && d.2.iter().all(|x| have.contains(x))).collect();
        let stuff: Vec<&str> = if !known.is_empty() && self.walk.rand() < tn().cook_known {
            known[(self.walk.rand() * known.len() as f32) as usize % known.len()].2.to_vec()
        } else {
            // something new: two or three things it hasn't tried together
            let mut pick = Vec::new();
            for _ in 0..8 {
                let mut pool = have.clone();
                let want = if pool.len() >= 3 && self.walk.rand() < 0.4 { 3 } else { 2 };
                let mut mix = Vec::new();
                for _ in 0..want {
                    let k = (self.walk.rand() * pool.len() as f32) as usize % pool.len();
                    mix.push(pool.swap_remove(k));
                }
                let mut sorted: Vec<String> = mix.iter().map(|x| x.to_string()).collect();
                sorted.sort();
                pick = mix;
                if !book.failed.contains(&sorted) {
                    break;
                }
            }
            pick
        };
        // the staff's share stays in the pantry
        if !hunger::pantry_spare(stuff.len() as u32) || !park::take_food(&stuff) {
            return None;
        }
        let dish = park::dish_of(stuff.clone());
        let skill = self.skill(cook, "cooking");
        // beginners burn things now and then, even what they know
        let chef = if self.is_job(cook, "chef") { 0.4 } else { 1.0 };
        let burnt = dish.is_none() || self.walk.rand() < (0.03 + 0.22 * (1.0 - skill)) * chef;
        let id = dish.unwrap_or("burnt").to_string();
        let new = dish.is_some() && !burnt && !book.found.contains_key(&id);
        Some(Meal { dish: if burnt { "burnt".into() } else { id }, new, burnt, stuff: stuff.iter().map(|x| x.to_string()).collect() })
    }

    /// On the farm: dig up what's ripe (or water it so it grows faster),
    /// take the eggs from the coop, milk the cows.
    fn play_farm(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, mid: f32, unit: f32, dt: f32) -> bool {
        let _ = c;
        let up = self.thing_up(t);
        for k in 0..p.riders.len() {
            let mut r = p.riders[k];
            let spot = mid + (k as f32 - 0.5) * 0.5 * unit;
            // up the meadow to it
            if r.phase == 0 {
                if self.walk_up_to(r.i, spot, up, dt) {
                    r.phase = 1;
                    r.t = 0.0;
                    self.set_face(r.i, spot > mid);
                }
                p.riders[k] = r;
                continue;
            }
            // done: back down to the floor
            if r.phase == 3 {
                if self.walk_up_to(r.i, spot + if spot > mid { 0.8 } else { -0.8 } * unit, 0.0, dt) {
                    r.phase = 4;
                }
                p.riders[k] = r;
                continue;
            }
            if r.phase == 4 {
                continue;
            }
            r.t += dt;
            if r.phase == 1 && r.t > 1.5 {
                r.phase = 2;
                let mut farm = park::read_farm();
                let (_, ready) = park::farm_state(t, &farm);
                let key = t.id.to_string();
                if ready > 0 && t.kind == "tree" {
                    // logs fallen under the tree: into the chest
                    let name = self.character_name(r.i);
                    for _ in 0..ready {
                        park::add(&name, "wood");
                    }
                    park::pay_upto("Park", ready, &name);
                    farm.insert(key, park::now());
                    park::write_farm(&farm);
                    log::debug!("{} picks up {ready} wood", r.i);
                    self.set_happy(r.i, true);

                } else if ready > 0 {
                    let (food, k) = match t.kind.as_str() {
                        "carrots" => ("carrot", 2),
                        "wheat" => ("wheat", 2),
                        "apple_tree" => ("apple", ready),
                        "coop" => ("egg", ready),
                        _ => ("milk", ready),
                    };
                    self.bring_food(r.i, food, k);
                    farm.insert(key, park::now());
                    park::write_farm(&farm);
                    self.work(r.i, "farmer", 2.0);
                    log::debug!("{} takes {k} {food}", r.i);
                    self.set_happy(r.i, true);

                } else {
                    // nothing yet: tends it, and it comes on a little faster
                    let push = match t.kind.as_str() {
                        "carrots" | "wheat" => park::CROP_GROW * 0.12,
                        "apple_tree" => park::APPLE_GROW * 0.1,
                        _ => 0.0,
                    };
                    let push = if self.is_job(r.i, "farmer") { push * 2.0 } else { push };
                    if push > 0.0 {
                        let since = farm.get(&key).copied().unwrap_or_else(park::now);
                        farm.insert(key, since - push);
                        park::write_farm(&farm);
                    }
                    self.work(r.i, "farmer", 1.0);

                }
            }

            if r.phase == 2 && r.t > 5.0 {
                r.phase = 3;
                self.set_happy(r.i, false);
            }
            p.riders[k] = r;
        }
        p.riders.iter().all(|r| r.phase == 4) || p.t > 60.0
    }

    /// Where those at a table sit: on the chairs beside it (x, height to sit
    /// at), then standing places round it.
    pub(super) fn table_seats(&self, t: &Thing, mid: f32, unit: f32) -> Vec<(f32, f32)> {
        let park = park_now();
        // each chair belongs to the nearest table only (no two at one chair)
        let tables: Vec<f32> = park.things.iter().filter(|o| o.kind == "table" && o.place == t.place).map(|o| self.thing_px(o)).collect();
        let mut chairs: Vec<f32> = park
            .things
            .iter()
            .filter(|c| c.kind == "chair" && c.place == t.place && (c.y - t.y).abs() < 0.02)
            .map(|c| self.thing_px(c))
            .filter(|x| (x - mid).abs() < 1.3 * unit)
            .filter(|x| tables.iter().all(|o| (x - o).abs() >= (x - mid).abs() - 0.5))
            .collect();
        chairs.sort_by(|a, b| (a - mid).abs().total_cmp(&(b - mid).abs()));
        let mut seats: Vec<(f32, f32)> = chairs.into_iter().take(4).map(|x| (x, park::SEAT * unit + 0.02 * unit)).collect();
        // then standing places round it, clear of chairs and of other tables' places
        let others: Vec<f32> = tables.iter().copied().filter(|o| (o - mid).abs() > 1.0).collect();
        for x in [mid - 0.85 * unit, mid + 0.85 * unit, mid - 1.3 * unit, mid + 1.3 * unit] {
            if seats.len() >= 4 {
                break;
            }
            let clear = !seats.iter().any(|s| (s.0 - x).abs() < 0.4 * unit) && others.iter().all(|o| (o - x).abs() > 0.85 * unit);
            if clear {
                seats.push((x, 0.0));
            }
        }
        seats
    }

    /// Carrying a dish: to the stove for it, over to the table, down it goes.
    fn play_serve(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, dt: f32) -> bool {
        let _ = c;
        let Some(mut r) = p.riders.first().copied() else { return true };
        let table = self.thing_px(t);
        let unit = self.park_unit(self.extras.len() + 1);
        r.t += dt;
        let go = |this: &mut Self, x: f32| {
            if !this.walk_ref(r.i).social.is_some_and(|s| s.moving) && (this.strip_px(r.i) - x).abs() > 8.0 && this.arrived(r.i) {
                this.go_facing(r.i, x, 0.2);
            }
            this.arrived(r.i) && (this.strip_px(r.i) - x).abs() <= 8.0
        };
        match r.phase {
            0 => {
                if go(self, p.from + 0.6 * unit) {
                    r.phase = 1;
                    r.t = 0.0;
                }
            }
            1 if r.t > 0.6 => {
                r.phase = 2;
                self.walk_of(r.i).start_bob(tn().jump_height * 0.12);
                // picked up off the counter
                if p.counter != 0 {
                    let dish = p.meal.as_ref().map(|m| m.dish.clone()).unwrap_or_default();
                    let mut l = LIVE.lock().unwrap();
                    if let Some(k) = l.dishes.iter().position(|d| d.0 == p.counter && d.1 == dish) {
                        l.dishes.remove(k);
                    }
                }
            }
            2 => {
                let spot = table + if p.from < table { -0.7 } else { 0.7 } * unit;
                if go(self, spot) {
                    let dish = p.meal.as_ref().map_or_else(|| "bread".to_string(), |m| m.dish.clone());
                    LIVE.lock().unwrap().dishes.push((t.id, dish, 1.0));
                    if p.diner != usize::MAX {
                        let w = r.i;
                        dine::set_order(p.diner, |o| {
                            o.state = dine::Ask::Served;
                            o.waiter = Some(w);
                        });
                        p.riders[0] = r;
                        self.work(r.i, "waiter", 2.0);
                        return true;
                    }
                    if !p.by_cook {
                        self.work(r.i, "waiter", 2.0);
                    }
                    self.set_happy(r.i, true);
                    // the one who cooked it eats it; a waiter only carried it
                    if p.by_cook {
                        PENDING.lock().unwrap().push((r.i, p.thing));
                    }
                    p.riders[0] = r;
                    return true;
                }
            }
            _ => {}
        }
        p.riders[0] = r;
        p.t > 40.0
    }

    /// The dish in the hands of whoever is carrying it.
    pub(super) fn serve_props(&self, v: &mut Vec<(i32, [f32; 4], bool, usize)>) {
        self.dine_props(v);
        self.snack_props(v);
        for c in self.chats.iter().filter(|c| c.stage == Stage::Play) {
            let Some(p) = c.play.as_ref().filter(|p| p.kind == "serve") else { continue };
            let Some(r) = p.riders.first().filter(|r| r.phase == 2) else { continue };
            let Some(k) = p.meal.as_ref().and_then(|m| park::DISHES.iter().position(|d| d.0 == m.dish)) else { continue };
            let rect = if r.i == 0 { self.hit_rect } else { self.extras[r.i - 1].hit_rect };
            let h = (rect[3] - rect[1]).max(1.0);
            let size = h * 0.3;
            let x = (rect[0] + rect[2]) * 0.5;
            // the waiter: on the tray on its head; anyone else: in its hands, at its middle
            let top = rect[1] + h * self.actor_ref(r.i).opaque[1];
            let waiter = self.is_job(r.i, "waiter") || self.is_part(r.i, "waiter");
            let tray = waiter && super::super::wear::wears(&self.character_name(r.i), "custom:tray");
            let y = if tray { top - size * 0.55 } else { self.hands_y(r.i) };
            v.push((super::FOOD + k as i32, [x - size * 0.5, y - size * 0.5, x + size * 0.5, y + size * 0.5], false, r.i));
        }
    }

    /// A dish left on the serving counter with nobody coming for it (its
    /// carrier was picked up, say): someone free in the cafe takes it to a
    /// table and eats it.
    fn leftovers(&mut self, chats: &mut Vec<Chat>, n: usize) {
        static SEEN: Mutex<Vec<(u64, String, Instant)>> = Mutex::new(Vec::new());
        let on_counters: Vec<(u64, String)> = {
            let park = park_now();
            let counters: Vec<u64> = park.things.iter().filter(|t| t.kind == "counter").map(|t| t.id).collect();
            LIVE.lock().unwrap().dishes.iter().filter(|d| counters.contains(&d.0)).map(|d| (d.0, d.1.clone())).collect()
        };
        let mut seen = SEEN.lock().unwrap();
        seen.retain(|s| on_counters.iter().any(|d| d.0 == s.0 && d.1 == s.1));
        for d in &on_counters {
            if !seen.iter().any(|s| s.0 == d.0 && s.1 == d.1) {
                seen.push((d.0, d.1.clone(), Instant::now()));
            }
        }
        // someone on the way for it, or a diner's order: not left over
        let wanted = |dish: &str| {
            chats.iter().filter_map(|c| c.play.as_ref()).any(|p| p.kind == "serve" && p.meal.as_ref().is_some_and(|m| m.dish == dish))
                || dine::ORDERS.lock().unwrap().iter().any(|o| o.dish == dish && o.state == dine::Ask::Ready)
        };
        let Some(old) = seen.iter().find(|s| s.2.elapsed().as_secs_f32() > 45.0 && !wanted(&s.1)).cloned() else { return };
        drop(seen);
        let park = park_now();
        let Some(counter) = park.things.iter().find(|t| t.id == old.0).cloned() else { return };
        let table = park.things.iter().enumerate().filter(|(_, t)| t.kind == "table" && t.place == counter.place && dish_on(t.id).is_none()).map(|(k, _)| k).next();
        let who = (0..n).find(|&i| self.walk_ref(i).at_leisure() && self.place_here(i).is_some_and(|p| p.place == counter.place));
        if let (Some(k), Some(i)) = (table, who) {
            log::debug!("cafe: {i} takes the left-over {} to a table", old.1);
            SEEN.lock().unwrap().retain(|s| !(s.0 == old.0 && s.1 == old.1));
            SERVE.lock().unwrap().push(Serve { who: i, table: k, dish: old.1, from: self.thing_px(&counter), counter: counter.id, by_cook: true, diner: None });
        }
    }

    /// A dish on a table with nobody at it for a while: someone hungry (or
    /// just free) in the room sits down to it; after five minutes it's cleared.
    fn table_leftovers(&mut self, chats: &mut Vec<Chat>, n: usize) {
        static SEEN: Mutex<Vec<(u64, Instant)>> = Mutex::new(Vec::new());
        let park = park_now();
        let tables: Vec<(usize, u64, String)> = park.things.iter().enumerate().filter(|(_, t)| t.kind == "table").map(|(k, t)| (k, t.id, t.place.clone())).collect();
        let dished: Vec<u64> = LIVE.lock().unwrap().dishes.iter().map(|d| d.0).collect();
        let mut seen = SEEN.lock().unwrap();
        // tables with a dish and nobody eating or carrying to them
        let alone: Vec<(usize, u64, String)> = tables
            .into_iter()
            .filter(|(k, id, _)| dished.contains(id) && !chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.thing == *k && matches!(p.kind.as_str(), "table" | "dine" | "serve"))))
            .collect();
        seen.retain(|s| alone.iter().any(|a| a.1 == s.0));
        for a in &alone {
            if !seen.iter().any(|s| s.0 == a.1) {
                seen.push((a.1, Instant::now()));
            }
        }
        let Some(&(id, since)) = seen.iter().find(|s| s.1.elapsed().as_secs_f32() > 30.0) else { return };
        let Some((k, _, place)) = alone.into_iter().find(|a| a.1 == id) else { return };
        if since.elapsed().as_secs_f32() > 300.0 {
            LIVE.lock().unwrap().dishes.retain(|d| d.0 != id);
            seen.retain(|s| s.0 != id);
            return;
        }
        drop(seen);
        let mut who: Vec<usize> = (0..n).filter(|&i| self.walk_ref(i).at_leisure() && self.place_here(i).is_some_and(|p| p.place == place)).collect();
        who.sort_by_key(|&i| !self.hungry(i));
        if let Some(&i) = who.first() {
            log::debug!("{i} sits down to a dish left on a table");
            self.start_play(chats, i, k, "table");
            SEEN.lock().unwrap().retain(|s| s.0 != id);
        }
    }




    /// A stroll up the meadow: a spot or two somewhere on it, a look round,
    /// then back down to the floor.
    fn play_meadow(&mut self, c: &mut Chat, p: &mut Play, dt: f32) -> bool {
        let i = c.members[0];
        let Some(farm) = governor::place_rect("farm") else { return true };
        let unit = self.park_unit(self.extras.len() + 1);
        if p.riders.is_empty() {
            p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 1 + (self.walk.rand() * 2.5) as u8 });
            p.meal = None;
        }
        p.t += dt;
        let mut r = p.riders[0];
        r.t += dt;
        let land = (farm.x + unit, farm.x + (1.0 - park::POND) * farm.w - unit);
        match r.phase {
            // pick somewhere up there
            0 => {
                let (a, b) = (self.walk.rand(), self.walk.rand());
                p.from = land.0 + a * (land.1 - land.0).max(0.0);
                p.stay = (0.05 + b * 0.9) * park::MEADOW * farm.h;
                r.phase = 1;
            }
            1 => {
                if self.walk_up_to(i, p.from, p.stay, dt) {
                    r.phase = 2;
                    r.t = 0.0;
                    r.n += 1;
                }
            }
            // a look round; then another spot, or home
            2 => {
                if self.walk.rand() < dt * 0.3 {
                    let wk = self.walk_of(i);
                    let left = wk.left;
                    if let Some(s) = wk.social.as_mut() {
                        s.face_left = !left;
                    }
                }
                if r.t > 4.0 + (r.n as f32) * 2.0 {
                    r.phase = if r.n >= r.want { 3 } else { 0 };
                    if r.phase == 3 {
                        let x = self.strip_px(i);
                        p.from = (x + if self.walk.rand() < 0.5 { -1.0 } else { 1.0 } * unit).clamp(land.0, land.1);
                    }
                }
            }
            _ => {
                if self.walk_up_to(i, p.from, 0.0, dt) {
                    return true;
                }
            }
        }
        p.riders[0] = r;
        p.t > 90.0
    }

    /// After a hen: it runs off, slower and slower, and once caught it hops
    /// and the chaser jumps for joy.
    fn play_chase(&mut self, c: &mut Chat, p: &mut Play, dt: f32) -> bool {
        let _ = c;
        p.t += dt;
        let i = p.riders.first().map_or(c.members[0], |r| r.i);
        if p.riders.is_empty() {
            p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 0 });
        }
        let unit = self.park_unit(self.extras.len() + 1);
        let farm = governor::place_rect("farm");
        let (x0, w, wh) = farm.map_or((0.0, self.view_w.max(1.0), 1000.0), |p| (p.x, p.w, p.h));
        // caught (or given up): back down to the floor
        if p.riders[0].phase == 1 {
            let x = self.strip_px(i);
            return self.walk_up_to(i, x + 0.6 * unit, 0.0, dt) || p.t > 25.0;
        }
        let me = self.strip_px(i);
        let mut l = LIVE.lock().unwrap();
        let Some(b) = l.beasts.get_mut(p.thing) else { return true };
        let (at, up) = (x0 + b.x * w, b.y * wh);
        let d = at - me;
        if d.abs() < unit * 2.2 {
            // runs off, tiring
            let tired = (1.0 - p.t / 9.0).max(0.15);
            b.flee = d.signum() * 0.09 * tired;
            b.flee_t = 0.3;
        }
        let caught = d.abs() < unit * 0.45 && p.t > 2.5;
        if caught {
            b.hop = 1.0;
            b.flee_t = 1.5;
            b.flee = d.signum() * 0.05;
        }
        drop(l);
        if caught || p.t > 14.0 {
            if caught {
                self.set_happy(i, true);
                self.walk_of(i).start_bob(tn().jump_height * 0.5);
                log::debug!("{i} caught a hen");
            }
            p.riders[0].phase = 1;
            return false;
        }
        self.walk_up_to(i, at, up, dt);
        false
    }

    /// Down in the sand, digging, moving to another corner now and then.
    fn play_sandbox(&mut self, c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
        let half = 0.55 * unit;
        for k in 0..p.riders.len() {
            let mut r = p.riders[k];
            r.t -= dt;
            if r.t <= 0.0 && self.arrived(r.i) {
                let spot = mid - half + self.walk.rand() * half * 2.0;
                self.go_facing(r.i, spot, 0.12);
                r.t = 4.0 + self.walk.rand() * 4.0;
                r.phase = 1;
            }
            if r.phase == 1 && self.arrived(r.i) {
                let wk = self.walk_of(r.i);
                wk.sink = tn().sit_sink * 1.2;
                if wk.rand() < dt * 1.5 {
                    wk.gesture = true;
                }
                let other = p.riders.iter().map(|o| o.i).find(|&o| o != r.i);
                self.set_happy(r.i, other.is_some());
            }
            p.riders[k] = r;
        }
        let _ = c;
        p.t > p.stay
    }
}

/// Cooks and those who waited, off to eat at a table: (who, table index).
static PENDING: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
/// Someone to carry a dish to a table.
struct Serve {
    who: usize,
    table: usize,
    dish: String,
    // where it's picked up (strip px), off which counter (0: the stove)
    from: f32,
    counter: u64,
    by_cook: bool,
    // a diner's order, when it's one
    diner: Option<usize>,
}

static SERVE: Mutex<Vec<Serve>> = Mutex::new(Vec::new());

/// Places taken at the farm's pond (who, x, up, when last seen there).
static SPOTS: Mutex<Vec<(usize, f32, f32, Instant)>> = Mutex::new(Vec::new());
/// Up the meadow with nothing left to do there: to walk back down.
static DOWN: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Picked by a right double click: (who, when).
static PICKED: Mutex<Option<(usize, Instant)>> = Mutex::new(None);
static ORDERS_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl CharacterOverlay {
    /// Right double click on a character picks it; then a right double click
    /// on the ground sends it there, on something in the park sends it to play.
    pub(super) fn right_orders(&mut self, chats: &mut Vec<Chat>, n: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        // picked but told nothing for 5 s: let go
        let expired = {
            let mut picked = PICKED.lock().unwrap();
            let gone = picked.filter(|p| p.1.elapsed().as_secs_f32() > 5.0).map(|p| p.0);
            if gone.is_some() {
                *picked = None;
            }
            gone
        };
        if let Some(i) = expired.filter(|&i| i < n) {
            tell(&format!("{} is no longer picked", self.character_name(i)));
        }
        let now = crate::backend::pointer::RIGHT_DOUBLE.load(Relaxed);
        if ORDERS_SEEN.swap(now, Relaxed) == now || now == 0 {
            return;
        }
        let m = self.screen_mouse;
        // on a character: that one listens
        let hit = (0..n).find(|&i| {
            let a = self.actor_ref(i);
            let r = a.body_box();
            let r = a.zoom.map_or(r, |z| z.to_screen(r));
            a.visible && m[0] >= r[0] && m[0] <= r[2] && m[1] >= r[1] && m[1] <= r[3]
        });
        if let Some(i) = hit {
            *PICKED.lock().unwrap() = Some((i, Instant::now()));
            let wk = self.walk_of(i);
            if matches!(wk.act, Act::Idle | Act::Walk) && wk.air <= 0.0 {
                wk.start_jump(tn().jump_height * 0.4);
            }
            log::debug!("orders: {i} picked");
            tell(&format!("{} is picked: right double click where it should go", self.character_name(i)));
            return;
        }
        let Some((i, at)) = PICKED.lock().unwrap().take() else { return };
        if at.elapsed().as_secs_f32() > 5.0 || i >= n {
            return;
        }
        let a = self.actor_ref(i);
        if !a.visible {
            return;
        }
        let p = a.zoom.map_or(m, |z| z.to_scene(m));
        let px = p[0] + a.own_view[0] * self.view_w * 0.5;
        // out of whatever it was doing
        for c in chats.iter_mut() {
            if c.members.contains(&i) || c.watchers.contains(&i) {
                c.members.retain(|&x| x != i);
                c.watchers.retain(|&x| x != i);
                self.let_down(i);
                self.leave(i, c.id);
            }
        }
        let wk = self.walk_ref(i);
        if !matches!(wk.act, Act::Idle | Act::Walk | Act::Rest) || wk.race.is_some() || wk.trip.is_some() {
            tell(&format!("{} can't go right now", self.character_name(i)));
            return;
        }
        if wk.act == Act::Rest {
            let wk = self.walk_of(i);
            wk.rest = None;
            wk.act = Act::Idle;
            wk.sink = 0.0;
            wk.rot = 0.0;
        }
        // something in the park under the click: play on it
        if let Some(place) = governor::place_at(self.walk_ref(i).ws, px).map(|p| p.place) {
            let park = park_now();
            let unit = self.park_unit(n);
            let ground = { let r = self.actor_ref(i).hit_rect; r[1] + (r[3] - r[1]) * self.actor_ref(i).opaque[2] };
            let on = park.things.iter().enumerate().filter(|(_, t)| park::room(&t.kind) > 0 && t.place == place).find(|(_, t)| {
                let base = ground - self.thing_up(t);
                (px - self.thing_px(t)).abs() <= park::width(t) * unit * 0.5 && p[1] >= base - 2.0 * unit && p[1] <= base + 0.3 * unit
            });
            if let Some((k, t)) = on {
                let kind = t.kind.clone();
                log::debug!("orders: {i} sent to the {kind}");
                let label = park::recipe(&kind).map_or(kind.clone(), |r| r.1.to_lowercase());
                tell(&format!("{} goes to the {label}", self.character_name(i)));
                self.start_play(chats, i, k, &kind);
                return;
            }
        }
        // just there
        let here = self.strip_px(i);
        let surface = self.actor_ref(i).surface[0].max(1.0);
        let wk = self.walk_of(i);
        wk.target = wk.x + (px - here) * 2.0 / surface;
        wk.act = Act::Walk;
        wk.timer = 3.0;
        log::debug!("orders: {i} walks to {px:.0}");
        tell(&format!("{} is on its way", self.character_name(i)));
    }

    /// What's drawn over the park on the character layer: the swing's ropes
    /// from the bar to the hands of whoever swings, and the seesaw's board
    /// under its two riders. (surface px, owner, half thickness px, twisted)
    pub(in crate::render::character) fn park_ropes(&self) -> Vec<([f32; 4], usize, f32, bool)> {
        let park = park_now();
        let n = self.extras.len() + 1;
        let unit = self.park_unit(n);
        let mut out = Vec::new();
        // (box, view offset, where the feet are drawn, the ground under them)
        let feet = |this: &Self, i: usize| {
            let rect = if i == 0 { this.hit_rect } else { this.extras[i - 1].hit_rect };
            let a = this.actor_ref(i);
            let h = (rect[3] - rect[1]).max(1.0);
            let soles = rect[1] + h * a.opaque[2];
            (rect, a.own_view[0] * this.view_w * 0.5, soles, soles + a.walk.air * h * 0.5)
        };
        for c in self.chats.iter().filter(|c| c.stage == Stage::Play) {
            let Some(p) = c.play.as_ref() else { continue };
            let Some(t) = park.things.get(p.thing) else { continue };
            if p.kind == "swing" {
                let Some(r) = p.riders.first().filter(|r| r.phase == 1) else { continue };
                let (rect, off, soles, ground) = feet(self, r.i);
                let h = (rect[3] - rect[1]).max(1.0);
                let top = ground - park::SWING_TOP * unit;
                let px = self.thing_px(t) - off;
                // down to a seat right under its feet
                let (hx, hy) = ((rect[0] + rect[2]) * 0.5, soles);
                let _ = h;
                for side in [-1.0, 1.0] {
                    let d = side * 0.18 * unit;
                    out.push(([px + d, top, hx + d, hy], r.i, 3.0, true));
                }
                out.push(([hx - 0.24 * unit, hy + 0.03 * unit, hx + 0.24 * unit, hy + 0.03 * unit], r.i, 0.035 * unit, false));
            } else if p.kind == "seesaw" && seesaw_rocking(p) {
                let (a, b) = (p.riders[0].i, p.riders[1].i);
                let (_, offa, ya, _) = feet(self, a);
                let (_, _, yb, _) = feet(self, b);
                let half = park::SEESAW_HALF * unit;
                let mid = self.thing_px(t) - offa;
                // the board runs through both riders' feet, a little past each
                let k = half / (half * 0.92);
                let (y0, y1) = ((ya + yb) * 0.5 + (ya - yb) * 0.5 * k, (ya + yb) * 0.5 - (ya - yb) * 0.5 * k);
                out.push(([mid - half, y0 + 4.0, mid + half, y1 + 4.0], a, 0.06 * unit, false));
            }
        }
        out
    }

    /// The windows' picture of what's going on: swings and seesaws in use,
    /// the animals, the stoves and the dishes. Written when it changes, at
    /// most five times a second (in the runtime dir, not on disk).
    pub(super) fn park_tell_world(&self) {
        let busy: Vec<usize> = self
            .chats
            .iter()
            .filter(|c| c.stage == Stage::Play)
            .filter_map(|c| c.play.as_ref())
            .filter(|p| (p.kind == "swing" && p.riders.first().is_some_and(|r| r.phase == 1)) || (p.kind == "seesaw" && seesaw_rocking(p)))
            .map(|p| p.thing)
            .collect();
        if LIVE.lock().unwrap().written.is_some_and(|t| t.elapsed().as_secs_f32() < 0.2) {
            return;
        }
        let hunger = self.hunger_list();
        let park = governor::place_rect("park");
        let mut l = LIVE.lock().unwrap();
        let world = park::World {
            busy,
            animals: l.beasts.iter().map(|b| park::Animal { kind: b.kind.to_string(), x: b.x, y: b.y, left: b.left, hop: b.hop.max(0.0) }).collect(),
            stoves: l.stoves.clone(),
            dishes: l.dishes.clone(),
            picnics: l.picnics.iter().filter_map(|p| park.as_ref().map(|k| ((p.0 - k.x) / k.w.max(1.0), p.1.clone(), p.2))).collect(),
            hunger,
        };
        if l.last.as_ref() == Some(&world) {
            return;
        }
        if let Ok(text) = serde_json::to_string(&world) {
            let _ = crate::util::atomic_write(&park::world_path(), text.as_bytes());
        }
        l.written = Some(Instant::now());
        l.last = Some(world);
    }

    /// Every frame: the hens and cows potter about (and run from whoever is
    /// after them), new beds start growing, and those who've cooked go to eat.
    pub(super) fn farm_tick(&mut self, chats: &mut Vec<Chat>, dt: f32, n: usize) {
        {
            static TIDY: Mutex<Option<Instant>> = Mutex::new(None);
            let mut t = TIDY.lock().unwrap();
            if t.is_none_or(|t| t.elapsed().as_secs_f32() > 10.0) {
                *t = Some(Instant::now());
                drop(t);
                let team: Vec<String> = (0..n).map(|i| self.character_name(i)).collect();
                let settled = self.social_age > 120.0 && n >= 2;
                for line in park::tidy_jobs(&team, settled) {
                    log::info!("{line}");
                    tell(&format!("{line}!"));
                }
            }
        }
        // on shift: kept out of chats (the wait is longer than any chat ignores)
        let keep_out = tn().overlap_cool + 5.0;
        let mut fisher_free = None;
        for i in 0..n {
            if self.walk_ref(i).social.is_none() && self.on_shift(i) {
                let wk = self.walk_of(i);
                wk.social_cool = wk.social_cool.max(keep_out);
                if wk.at_leisure() && wk.act == Act::Idle && wk.fish_cool <= 0.0 && !self.hungry(i) && self.may_work(i, "fisher") && (self.is_job(i, "fisher") || self.is_part(i, "fisher")) {
                    fisher_free = Some(i);
                }
            }
        }
        // the fisher on shift: off to fish (not off the meadow)
        if let Some(i) = fisher_free.filter(|&i| self.walk_ref(i).air <= 0.0) {
            log::debug!("{i} fishes on its shift");
            let c = self.fish_alone(i, false);
            chats.push(c);
        }
        self.restaurant_tick(chats);
        self.work_calls(chats, n);
        self.idle_nudge(chats, dt, n);
        self.host_help(n);
        self.shop_tick(chats, n);
        self.hunger_tick(chats, dt, n);
        self.visit_tick(chats, dt, n);
        self.leftovers(chats, n);
        self.table_leftovers(chats, n);
        self.game_time(chats, n);
        self.farm_calls(chats, n);
        // down from up the meadow, on foot
        let down: Vec<usize> = std::mem::take(&mut *DOWN.lock().unwrap());
        for i in down.into_iter().filter(|&i| i < n) {
            if self.walk_ref(i).social.is_some() || self.walk_ref(i).air <= 0.05 {
                continue;
            }
            self.start_play(chats, i, usize::MAX, "down");
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.perched = true;
            }
        }
        // a dish to carry: off to the stove for it
        let serve: Vec<Serve> = std::mem::take(&mut *SERVE.lock().unwrap());
        for Serve { who: i, table: k, dish, from, counter, by_cook, diner } in serve {
            if i >= n {
                continue;
            }
            // out of whatever it's in (the stove's chat has just ended anyway)
            for c in chats.iter_mut() {
                if c.members.contains(&i) {
                    c.members.retain(|&x| x != i);
                    self.let_down(i);
                    self.leave(i, c.id);
                }
            }
            self.start_play(chats, i, k, "serve");
            if let Some(c) = chats.iter_mut().rev().find(|c| c.members.contains(&i) && c.play.as_ref().is_some_and(|p| p.kind == "serve")) {
                if let Some(p) = c.play.as_mut() {
                    p.meal = Some(Meal { dish, new: false, burnt: false, stuff: Vec::new() });
                    p.from = from;
                    p.counter = counter;
                    p.by_cook = by_cook;
                    p.diner = diner.unwrap_or(usize::MAX);
                }
            }
        }
        // off to the table where the food is, with a friend or two
        let eat: Vec<(usize, usize)> = std::mem::take(&mut *PENDING.lock().unwrap());
        for (i, k) in eat {
            if i < n && self.walk_ref(i).at_leisure() {
                self.start_play(chats, i, k, "table");
            }
        }
        let farm_open = governor::place_workspace("farm").is_some();
        let park = park_now();
        let mut l = LIVE.lock().unwrap();
        if !farm_open {
            l.beasts.clear();
            return;
        }
        // a coop has three hens, a barn two cows
        let homes: Vec<(u64, &'static str, usize, f32, f32)> = park
            .things
            .iter()
            .filter(|t| t.place == "farm")
            .filter_map(|t| match t.kind.as_str() {
                "coop" => Some((t.id, "chicken", 3, t.x, t.y)),
                "barn" => Some((t.id, "cow", 2, t.x, t.y)),
                _ => None,
            })
            .collect();
        l.beasts.retain(|b| homes.iter().any(|h| h.0 == b.home));
        for &(id, kind, want, x, y) in &homes {
            let have = l.beasts.iter().filter(|b| b.home == id).count();
            for _ in have..want {
                let r = live_rand(&mut l);
                l.beasts.push(Beast { home: id, kind, x: (x + (r - 0.5) * 0.1).clamp(0.03, park::PIER.0 - 0.05), goal: x, left: r < 0.5, wait: r * 3.0, hop: 0.0, y: (y + (r - 0.5) * 0.03).max(0.0), goal_y: y, flee: 0.0, flee_t: 0.0 });
            }
        }
        let homes_x: Vec<(u64, f32)> = homes.iter().map(|h| (h.0, h.3)).collect();

        let (lo, hi) = (0.03, park::PIER.0 - 0.05);
        for k in 0..l.beasts.len() {
            let r1 = live_rand(&mut l);
            let r2 = live_rand(&mut l);
            let b = &mut l.beasts[k];
            b.hop = (b.hop - dt * 2.5).max(0.0);
            if b.flee_t > 0.0 {
                b.flee_t -= dt;
                b.x = (b.x + b.flee * dt).clamp(lo, hi);
                b.left = b.flee < 0.0;
                // hops as it runs
                if b.hop <= 0.0 {
                    b.hop = 0.6;
                }
                continue;
            }
            if b.wait > 0.0 {
                b.wait -= dt;
                // a peck at the ground now and then
                if b.kind == "chicken" && r1 < dt * 0.5 {
                    b.hop = 0.35;
                }
                continue;
            }
            // free over the whole meadow: across and up and down it
            let speed = if b.kind == "chicken" { 0.025 } else { 0.007 };
            let (dx, dy) = (b.goal - b.x, (b.goal_y - b.y) * 1.8);
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= speed * dt {
                b.x = b.goal;
                b.y = b.goal_y;
                b.wait = 1.0 + r1 * 5.0;
                let r3 = live_rand(&mut l);
                let r4 = live_rand(&mut l);
                let b = &mut l.beasts[k];
                // mostly a short walk; now and then off across the meadow
                let far = r3 < 0.3;
                let (rx, ry) = if far { (0.5, 0.5) } else if b.kind == "chicken" { (0.12, 0.1) } else { (0.15, 0.08) };
                b.goal = (if far { lo + r2 * (hi - lo) } else { b.x + (r2 - 0.5) * 2.0 * rx }).clamp(lo, hi);
                b.goal_y = (if far { r4 * park::MEADOW * 0.95 } else { b.y + (r4 - 0.5) * 2.0 * ry }).clamp(0.0, park::MEADOW * 0.95);
            } else {
                b.x += dx / dist * speed * dt;
                b.y += dy / dist * speed * dt / 1.8;
                if dx.abs() > 0.001 {
                    b.left = dx < 0.0;
                }
            }
        }
        let _ = &homes_x;
        drop(l);
        // anything newly built on the farm starts growing now
        let mut farm = park::read_farm_cached();
        let mut changed = false;
        for t in park.things.iter().filter(|t| (t.place == "farm" && matches!(t.kind.as_str(), "carrots" | "wheat" | "apple_tree" | "coop" | "barn")) || t.kind == "tree") {
            let key = t.id.to_string();
            if t.id != 0 && !farm.contains_key(&key) {
                farm.insert(key, park::now());
                changed = true;
            }
        }
        if changed {
            park::write_farm(&farm);
        }
    }
}

/// Both on the seesaw and going up and down.
fn seesaw_rocking(p: &Play) -> bool {
    p.riders.len() >= 2 && p.riders[0].phase == 1 && p.riders[1].phase == 1
}

/// A short desktop notification about orders, replacing the last one.
fn tell(text: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "Anima", "-t", "2500", "-h", "string:x-canonical-private-synchronous:anima-orders", "Anima", text])
        .spawn()
        .map(|mut c| std::thread::spawn(move || c.wait()));
}
