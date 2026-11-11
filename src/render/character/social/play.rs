//! Playing in the park: down the slide, on the swing and the seesaw, sitting
//! on chairs and benches, talking round the table, cooking at the stove,
//! digging in the sandbox. What's built comes from the park window.

use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use super::*;
use crate::render::character::park::{self, Park, Thing};

pub(super) struct Play {
    // which thing in the park list, and what it was when they started
    thing: usize,
    kind: String,
    riders: Vec<Rider>,
    t: f32,
    // how long this go lasts
    stay: f32,
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
        if governor::park_workspace().is_none() || self.view_w < 1.0 {
            return;
        }
        let frac = self.park_unit(n) / self.view_w;
        let mut c = CACHE.lock().unwrap();
        if (frac - c.unit_written).abs() > c.unit_written * 0.03 {
            c.unit_written = frac;
            let text = format!("{{\"unit_frac\": {frac:.5}}}");
            let _ = crate::util::atomic_write(&park::view_path(), text.as_bytes());
        }
    }

    fn thing_px(&self, t: &Thing) -> f32 {
        t.x * self.view_w
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
        let Some(ws) = governor::park_workspace() else { return };
        let park = park_now();
        if park.things.is_empty() {
            return;
        }
        let who: Vec<usize> = (0..n)
            .filter(|&i| {
                let wk = self.walk_ref(i);
                wk.ws == ws && wk.at_leisure() && wk.trip.is_none() && wk.rest.is_none() && chat_cool(wk) <= tn().overlap_cool && wk.energy >= tn().energy_watch_need
            })
            .collect();
        if who.is_empty() {
            return;
        }
        let i = who[(self.walk.rand() * who.len() as f32) as usize % who.len()];
        if self.walk_of(i).rand() >= tn().park_play {
            return;
        }
        // something with room; somewhere a friend is waiting counts for more
        let mut options: Vec<(usize, f32)> = Vec::new();
        for (k, t) in park.things.iter().enumerate() {
            let room = park::room(&t.kind);
            if room == 0 {
                continue;
            }
            let on = chats.iter().find(|c| c.stage == Stage::Play && c.play.as_ref().is_some_and(|p| p.thing == k && p.kind == t.kind));
            let used = on.map_or(0, |c| c.members.len());
            if used >= room {
                continue;
            }
            let waiting = matches!(t.kind.as_str(), "seesaw" | "table" | "bench") && used == 1;
            options.push((k, if waiting { 3.0 } else { 1.0 }));
        }
        let total: f32 = options.iter().map(|o| o.1).sum();
        if total <= 0.0 {
            return;
        }
        let mut roll = self.walk.rand() * total;
        let Some(&(k, _)) = options.iter().find(|o| {
            roll -= o.1;
            roll <= 0.0
        }) else {
            return;
        };
        let kind = park.things[k].kind.clone();
        self.start_play(chats, i, k, &kind);
    }

    /// `i` goes to play on thing `k`, with whoever is on it already.
    fn start_play(&mut self, chats: &mut Vec<Chat>, i: usize, k: usize, kind: &str) {
        let kind = kind.to_string();
        if let Some(c) = chats.iter_mut().find(|c| c.stage == Stage::Play && c.play.as_ref().is_some_and(|p| p.thing == k && p.kind == kind)) {
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
        c.play = Some(Play { thing: k, kind: kind.clone(), riders: Vec::new(), t: 0.0, stay });
        log::debug!("chat {id}: {i} goes to the {kind}");
        chats.push(c);
    }

    pub(super) fn run_play(&mut self, c: &mut Chat, dt: f32) -> bool {
        let Some(mut p) = c.play.take() else { return false };
        let park = park_now();
        let thing = park.things.get(p.thing).filter(|t| t.kind == p.kind).cloned();
        let Some(thing) = thing.filter(|_| governor::park_workspace().is_some()) else {
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
        let done = match thing.kind.as_str() {
            "slide" => self.play_slide(c, &mut p, &thing, mid, unit, dt),
            "swing" => self.play_swing(c, &mut p, mid, unit, dt),
            "seesaw" => self.play_seesaw(c, &mut p, mid, unit, dt),
            "chair" | "bench" => self.play_sit(c, &mut p, &thing, mid, unit),
            "table" => self.play_table(c, &mut p, mid, unit, dt),
            "stove" => self.play_stove(c, &mut p, mid, unit, dt),
            "sandbox" => self.play_sandbox(c, &mut p, mid, unit, dt),
            _ => true,
        };
        if done {
            for &i in &c.members.clone() {
                self.let_down(i);
            }
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

    /// Round the table, chatting; alone it waits a while for company.
    fn play_table(&mut self, c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
        let spots = [mid - 0.85 * unit, mid + 0.85 * unit, mid - 1.3 * unit, mid + 1.3 * unit];
        for k in 0..p.riders.len().min(4) {
            let i = p.riders[k].i;
            if p.riders[k].phase == 0 {
                let spot = spots[k];
                if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - spot).abs() > 6.0 && self.arrived(i) {
                    self.go(i, spot, 0.14);
                } else if self.arrived(i) && (self.strip_px(i) - spot).abs() <= 6.0 {
                    p.riders[k].phase = 1;
                    self.set_face(i, spot > mid);
                }
            }
        }
        if c.members.len() >= 2 {
            self.chatter(c, dt);
        } else if p.t > 15.0 {
            return true;
        }
        p.t > p.stay
    }

    /// Cooking: busy hands and little hops, then a happy jump when it's ready;
    /// whoever came to wait cheers.
    fn play_stove(&mut self, c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
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
            if k == 0 {
                if self.walk.rand() < dt * 1.2 {
                    self.walk_of(i).gesture = true;
                }
                if self.walk.rand() < dt * 0.8 {
                    self.walk_of(i).start_bob(tn().jump_height * 0.08);
                }
            } else {
                self.face(i, p.riders[0].i);
            }
        }
        if p.t > p.stay && p.riders[0].phase == 1 {
            for r in p.riders.clone() {
                let wk = self.walk_of(r.i);
                wk.start_jump(tn().jump_height * 0.7);
                let secs = tn().laugh_time;
                wk.laugh(secs);
            }
            return true;
        }
        let _ = c;
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

/// Picked by a right double click: (who, when).
static PICKED: Mutex<Option<(usize, Instant)>> = Mutex::new(None);
static ORDERS_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl CharacterOverlay {
    /// Right double click on a character picks it; then a right double click
    /// on the ground sends it there, on something in the park sends it to play.
    pub(super) fn right_orders(&mut self, chats: &mut Vec<Chat>, n: usize) {
        use std::sync::atomic::Ordering::Relaxed;
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
            return;
        }
        let Some((i, at)) = PICKED.lock().unwrap().take() else { return };
        if at.elapsed().as_secs_f32() > 30.0 || i >= n {
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
        let park_ws = governor::park_workspace();
        if park_ws.is_some() && park_ws == Some(self.walk_ref(i).ws) {
            let park = park_now();
            let unit = self.park_unit(n);
            let ground = { let r = self.actor_ref(i).hit_rect; r[1] + (r[3] - r[1]) * self.actor_ref(i).opaque[2] };
            let on = park.things.iter().enumerate().filter(|(_, t)| park::room(&t.kind) > 0).find(|(_, t)| {
                (px - self.thing_px(t)).abs() <= park::width(t) * unit * 0.5 && p[1] >= ground - 2.0 * unit
            });
            if let Some((k, t)) = on {
                let kind = t.kind.clone();
                log::debug!("orders: {i} sent to the {kind}");
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

    /// Which swings and seesaws are in use, for the park window (it hides
    /// their resting ropes and board then). Written when that changes.
    pub(super) fn park_tell_busy(&self) {
        let busy: Vec<usize> = self
            .chats
            .iter()
            .filter(|c| c.stage == Stage::Play)
            .filter_map(|c| c.play.as_ref())
            .filter(|p| (p.kind == "swing" && p.riders.first().is_some_and(|r| r.phase == 1)) || (p.kind == "seesaw" && seesaw_rocking(p)))
            .map(|p| p.thing)
            .collect();
        static LAST: Mutex<Option<Vec<usize>>> = Mutex::new(None);
        let mut last = LAST.lock().unwrap();
        if last.as_ref() != Some(&busy) {
            let text = serde_json::json!({ "busy": busy }).to_string();
            let _ = crate::util::atomic_write(&park::live_path(), text.as_bytes());
            *last = Some(busy);
        }
    }
}

/// Both on the seesaw and going up and down.
fn seesaw_rocking(p: &Play) -> bool {
    p.riders.len() >= 2 && p.riders[0].phase == 1 && p.riders[1].phase == 1
}
