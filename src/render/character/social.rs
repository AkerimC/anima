//! Characters taking notice of each other: two standing close look over,
//! step side by side, chat for a while, maybe stroll together, then go their
//! own ways. Others passing by can join in, the more likely the bigger the
//! crowd, and a chat that gathers most of the team turns into a race. Only standing ones start one: walking through each
//! other is fine.

use crate::render::tuning::get as tn;
use super::CharacterOverlay;
use super::walk::{Act, WalkState};

/// A game asked for from the settings window (`anima game <name>`).
pub static GAME_NOW: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// One character's part in a chat. Goals are strip positions in logical px,
/// like a race's.
#[derive(Clone, Copy, Debug)]
pub(super) struct Social {
    chat: u32,
    pub(super) goal_px: f32,
    pub(super) goal: f32,
    speed: f32,
    moving: bool,
    face_left: bool,
    // in the happy pose (talking, laughing)
    pub(super) happy: bool,
    // playing the cursor game, and seconds to its next try
    pub(super) chasing: bool,
    hop_cool: f32,
    // long jump: in the air (spinning this fast), or planted in the ground
    pub(super) flying: bool,
    spin: f32,
    stuck: bool,
    rot_target: f32,
    sink_target: f32,
    bump_cool: f32,
    // the referee knows about this landing
    queued: bool,
    // where it took off from, logical px
    launch_px: f32,
    // leaning back on the rope (radians)
    lean: f32,
    // went over from the feet, like a plank, not spinning about its middle
    feet: bool,
    // tag: speeding up and slowing down (multiplier, where it's heading, seconds left)
    pace: f32,
    pace_goal: f32,
    pace_wait: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Notice,
    Approach,
    Talk,
    Stroll,
    // games: each jumps in turn, or all go after the cursor
    Contest,
    Cursor,
    Long,
    Domino,
    Tug,
    Tag,
    Cheer,
    Part,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Game {
    Contest,
    Cursor,
    Long,
    Domino,
    Tug,
    Tag,
}

impl Chat {
    fn new(id: u32, members: Vec<usize>, stage: Stage, timer: f32) -> Self {
        let talker = members[0];
        Chat { id, members, stage, timer, talker, turn: 0.0, bob: 0.0, strolls: 0, joined_walking: false, quiet: 0.0, games: 0, played: Vec::new(), watchers: Vec::new(), spots: Vec::new(), base: Vec::new(), offset: 0.0, pull: 0.0, pull_target: 0.0, it: 0, chase: 0, teams: (Vec::new(), Vec::new()), queue: Vec::new(), step: 0, wait: 0.0, scores: Vec::new(), winner: None, long: None, regroup: false, forced: None, recent: Vec::new() }
    }
}

/// One go in the long jump: alone, or thrown by a partner.
#[derive(Clone, Copy, Debug)]
struct Turn {
    flyer: usize,
    thrower: Option<usize>,
}

struct Long {
    referee: usize,
    line: f32,
    dir: f32,
    ready: bool,
    turns: Vec<Turn>,
    next: usize,
    gap: f32,
    // on their way to the line
    running: Vec<Turn>,
    // planted and not looked at yet; the one being looked at, seconds left
    landed: Vec<usize>,
    judging: Option<(usize, f32)>,
    // throwers done so far, standing out of the way past the line
    thrown: f32,
}

pub(super) struct Chat {
    id: u32,
    members: Vec<usize>,
    stage: Stage,
    timer: f32,
    // whose turn to talk, seconds left of it, seconds to the next nod
    talker: usize,
    turn: f32,
    bob: f32,
    strolls: u8,
    // someone joined on the walk: a full talk when they get there
    joined_walking: bool,
    // seconds since anyone joined, and whether this chat has had its game
    quiet: f32,
    games: u8,
    played: Vec<Game>,
    // saw the game and came over: they join when it's done
    watchers: Vec<usize>,
    // the other games: places, where each one started, how far the rope has
    // moved, who's "it" (tag), the two sides of the rope
    spots: Vec<f32>,
    base: Vec<(usize, f32)>,
    offset: f32,
    // rope speed and the speed it's heading for, logical px/s
    pull: f32,
    pull_target: f32,
    it: usize,
    // who "it" is after, for now
    chase: usize,
    teams: (Vec<usize>, Vec<usize>),
    // jumping game: who's still to jump, what they're doing, how long for
    queue: Vec<usize>,
    step: u8,
    wait: f32,
    // best so far: jump height or seconds on the cursor, per member
    scores: Vec<(usize, f32)>,
    winner: Option<usize>,
    long: Option<Long>,
    // scattered by a game: get back together before talking
    regroup: bool,
    // started from the settings window: play this as soon as they're lined up
    forced: Option<Game>,
    // tag: who was "it" just now and can't be tagged straight back
    recent: Vec<usize>,
}

/// How many in one chat set off a race: a minimum, and in a bigger team a
/// share of it.
fn race_at(team: usize) -> usize {
    let t = tn();
    ((team as f32 * t.race_share).ceil() as usize).max(t.race_min as usize)
}

/// Chance per look that someone nearby joins a chat this big: a pair is
/// hard to join, a crowd draws people in.
fn join_chance(members: usize) -> f32 {
    let t = tn();
    match members {
        0..=2 => t.join_2,
        3 => t.join_3,
        4 => t.join_4,
        5 => t.join_5,
        6 => t.join_6,
        7 => t.join_7,
        _ => t.join_8,
    }
}

fn join_range(members: usize) -> f32 {
    (tn().join_range + members.saturating_sub(2) as f32 * tn().join_range_add).min(tn().join_range_max)
}

/// Seconds before it's up for a chat; a rest after a crowd counts for more
/// than anything that gets past the usual pause.
fn chat_cool(wk: &WalkState) -> f32 {
    if wk.crowd_rest > 0.0 {
        wk.social_cool.max(wk.crowd_rest + tn().overlap_cool + 1.0)
    } else {
        wk.social_cool
    }
}

/// How many play a game started by hand, if the team has that many.
const FORCED_PLAYERS: usize = 6;

fn grounded(wk: &WalkState) -> bool {
    matches!(wk.act, Act::Idle | Act::Walk)
}

impl WalkState {
    /// In a chat, on the ground: walk to the goal or stand facing the others.
    pub(super) fn social_step(&mut self, dt: f32) -> Option<(f32, bool, f32)> {
        if !grounded(self) {
            return None;
        }
        let r = self.social.as_mut()?;
        if r.moving {
            let d = r.goal - self.x;
            let s = r.speed * dt;
            if d.abs() > s {
                self.x += d.signum() * s;
                self.left = d < 0.0;
                self.target = r.goal;
                self.act = Act::Walk;
                return Some((self.x, true, 0.0));
            }
            self.x = r.goal;
            r.moving = false;
        }
        self.left = r.face_left;
        self.act = Act::Idle;
        self.timer = 1.0;
        self.target = self.x;
        Some((self.x, false, 0.0))
    }

    /// Landed from a long jump: stuck in the ground, flat out or head first.
    pub(super) fn plant(&mut self) {
        use std::f32::consts::{FRAC_PI_2, PI};
        let roll = self.rand();
        let (rot, sink) = if roll < tn().long_head_first {
            (PI, 0.55)
        } else if roll < 0.7 {
            (FRAC_PI_2, 0.25)
        } else {
            (-FRAC_PI_2, 0.25)
        };
        self.act = Act::Idle;
        self.timer = 1.0;
        self.target = self.x;
        if let Some(r) = self.social.as_mut() {
            r.flying = false;
            r.stuck = true;
            r.rot_target = rot;
            r.sink_target = sink;
        }
    }

    /// Spin in the air, stay as planted, otherwise get back upright.
    fn update_pose(&mut self, dt: f32) {
        use std::f32::consts::{PI, TAU};
        match self.social {
            Some(s) if s.flying => {
                self.rot += s.spin * dt;
                self.feet = false;
            }
            Some(s) if s.stuck => {
                self.feet = s.feet;
                let k = (dt * if s.feet { 9.0 } else { 14.0 }).min(1.0);
                self.rot += ((s.rot_target - self.rot + PI).rem_euclid(TAU) - PI) * k;
                self.sink += (s.sink_target - self.sink) * k;
            }
            _ => {
                let k = (dt * 8.0).min(1.0);
                let lean = self.social.map_or(0.0, |s| s.lean);
                self.rot = (self.rot + PI).rem_euclid(TAU) - PI;
                self.rot += (lean - self.rot) * k;
                self.sink -= self.sink * k;
                if self.rot.abs() < 0.02 {
                    self.feet = false;
                }
            }
        }
    }

    /// A short quick bob, for talking and laughing.
    fn start_bob(&mut self, amp: f32) {
        self.start_jump(amp);
        self.jump_dur = 0.32;
        self.jump = 0.32;
    }
}

impl CharacterOverlay {
    /// Forget all chats (the team changed, indices don't hold any more).
    pub(super) fn clear_chats(&mut self) {
        self.chats.clear();
        self.new_chats.clear();
        self.walk.social = None;
        for e in &mut self.extras {
            e.walk.social = None;
        }
    }

    pub(super) fn run_social(&mut self, dt: f32, mouse: [f32; 2]) {
        let n = self.extras.len() + 1;
        for i in 0..n {
            let wk = self.walk_of(i);
            wk.social_cool = (wk.social_cool - dt).max(0.0);
            wk.dropped = (wk.dropped - dt).max(0.0);
            wk.crowd_rest = (wk.crowd_rest - dt).max(0.0);
            wk.update_pose(dt);
        }
        let music = self.vibe.on;
        let mut chats = std::mem::take(&mut self.chats);
        let asked = GAME_NOW.lock().unwrap().take();
        if let Some(name) = asked {
            self.force_game(&mut chats, &name, n);
        }
        chats.retain_mut(|c| {
            let keep = !music && self.prune(c, n) && self.advance(c, dt, mouse);
            if !keep {
                self.finish(c, n);
            }
            keep
        });

        chats.append(&mut self.new_chats);

        self.social_tick -= dt;
        if self.social_tick <= 0.0 {
            self.social_tick = tn().look_every;
            if self.race.is_none() && !music {
                for c in chats.iter_mut().filter(|c| matches!(c.stage, Stage::Contest | Stage::Cursor | Stage::Long | Stage::Domino | Stage::Tug | Stage::Tag | Stage::Cheer)) {
                    self.try_watch(c, n);
                }
                for c in chats.iter_mut() {
                    let full = c.members.len() >= tn().max_group as usize;
                    if !matches!(c.stage, Stage::Notice | Stage::Stroll | Stage::Long | Stage::Domino | Stage::Tug | Stage::Tag | Stage::Part) && self.overlap_join(c, n, full) {
                        continue;
                    }
                    if !full && matches!(c.stage, Stage::Talk | Stage::Stroll) {
                        self.try_join(c, n);
                    }
                }
                let room = chats.len() < (n / (tn().chat_divisor as usize).max(1)).max(1);
                chats.extend(self.find_pair(n, room));
            }
        }
        self.chats = chats;
    }

    /// Drop members who were picked up, called away and so on; false when
    /// too few are left to go on.
    fn prune(&mut self, c: &mut Chat, n: usize) -> bool {
        let id = c.id;
        let game = matches!(c.stage, Stage::Cursor | Stage::Long | Stage::Tug | Stage::Cheer);
        let mut left = Vec::new();
        c.members.retain(|&i| {
            let stays = i < n && {
                let wk = self.walk_ref(i);
                // hanging off the cursor and dropping from it is the game
                // (not when someone's picked it up: that's the button held down)
                let carried = crate::backend::pointer::BUTTON_DOWN.load(std::sync::atomic::Ordering::Relaxed);
                let playing = game && (wk.act == Act::Fall || (wk.act == Act::Held && (wk.hang > 0.0 || !carried)));
                wk.social.is_some_and(|s| s.chat == id) && !wk.circle && (playing || !matches!(wk.act, Act::Held | Act::Fall | Act::Pat | Act::Chase))
            };
            if !stays {
                left.push(i);
            }
            stays
        });
        for i in left.into_iter().filter(|&i| i < n) {
            self.leave(i, id);
        }
        let mut gone = Vec::new();
        c.watchers.retain(|&i| {
            let stays = i < n && {
                let wk = self.walk_ref(i);
                wk.social.is_some_and(|s| s.chat == id) && !wk.circle && !matches!(wk.act, Act::Held | Act::Fall | Act::Pat | Act::Chase)
            };
            if !stays {
                gone.push(i);
            }
            stays
        });
        for i in gone.into_iter().filter(|&i| i < n) {
            self.leave(i, id);
        }
        if !c.members.contains(&c.talker) {
            c.turn = 0.0;
        }
        c.members.len() >= 2
    }

    fn leave(&mut self, i: usize, id: u32) {
        let wk = self.walk_of(i);
        if wk.social.is_some_and(|s| s.chat == id) {
            wk.social = None;
            wk.social_cool = tn().cool_base + wk.rand() * tn().cool_rand;
        }
    }

    fn finish(&mut self, c: &Chat, n: usize) {
        let big = c.members.len() >= tn().big_group as usize;
        for &i in c.watchers.iter().filter(|&&i| i < n) {
            self.leave(i, c.id);
        }
        for &i in c.members.iter().filter(|&&i| i < n) {
            self.leave(i, c.id);
            if big {
                let wk = self.walk_of(i);
                wk.crowd_rest = tn().crowd_rest_base + wk.rand() * tn().crowd_rest_rand;
            }
        }
    }

    fn width_of(&self, i: usize) -> f32 {
        let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        (r[2] - r[0]).max(1.0)
    }

    fn on_screen(&self, i: usize) -> bool {
        let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        // an empty box: not drawn yet
        r[2] > r[0] && r[2] >= 0.0 && r[0] <= self.view_w
    }

    fn enlist(&mut self, i: usize, id: u32) {
        let wk = self.walk_of(i);
        let face_left = wk.left;
        wk.social = Some(Social { chat: id, goal_px: 0.0, goal: 0.0, speed: 0.0, moving: false, face_left, happy: false, chasing: false, hop_cool: 0.0, flying: false, spin: 0.0, stuck: false, rot_target: 0.0, sink_target: 0.0, bump_cool: 0.0, queued: false, launch_px: 0.0, lean: 0.0, feet: false, pace: 1.0, pace_goal: 1.0, pace_wait: 0.0 });
    }

    /// Two standing close, on screen and free: maybe they notice each other.
    /// `room`: not too many chats going on (standing inside each other is
    /// worth one anyway).
    fn find_pair(&mut self, n: usize, room: bool) -> Option<Chat> {
        let free: Vec<(usize, f32, f32, f32)> = (0..n)
            .filter(|&i| {
                let wk = self.walk_ref(i);
                wk.act == Act::Idle && wk.social.is_none() && wk.race.is_none() && !wk.circle && self.on_screen(i)
            })
            .map(|i| (i, self.strip_px(i), self.width_of(i), chat_cool(self.walk_ref(i))))
            .collect();
        let mut overlap: Option<(f32, usize, usize)> = None;
        let mut near: Option<(f32, usize, usize)> = None;
        let mut crowded: Option<(f32, usize, usize)> = None;
        for (k, p) in free.iter().enumerate() {
            for q in &free[k + 1..] {
                let d = (p.1 - q.1).abs();
                let size = (p.2 + q.2) * 0.5;
                let slot = if d < size * tn().overlap {
                    if p.3 <= tn().overlap_cool && q.3 <= tn().overlap_cool { &mut overlap } else { &mut crowded }
                } else if d < size * tn().near_range && p.3 <= 0.0 && q.3 <= 0.0 {
                    &mut near
                } else {
                    continue;
                };
                if slot.is_none_or(|s| d < s.0) {
                    *slot = Some((d, p.0, q.0));
                }
            }
        }
        let roll = self.walk.rand();
        let pair = match (overlap, near) {
            (Some(o), _) if roll < tn().overlap_chance => o,
            (_, Some(o)) if room && roll < tn().near_chance => o,
            _ => {
                if let Some((d, a, b)) = crowded {
                    if self.walk.rand() < tn().step_chance {
                        let (i, other) = if self.walk.rand() < 0.5 { (a, b) } else { (b, a) };
                        self.step_aside(i, other, d);
                    }
                }
                return None;
            }
        };
        // either may be the one who notices first
        let (a, b) = if self.walk.rand() < 0.5 { (pair.1, pair.2) } else { (pair.2, pair.1) };
        self.chat_id = self.chat_id.wrapping_add(1);
        let id = self.chat_id;
        self.enlist(a, id);
        self.enlist(b, id);
        let timer = 1.2 + self.walk.rand() * 0.8;
        self.face(a, b);
        log::debug!("chat {id}: {a} and {b}{}", if pair.0 < self.width_of(a) * tn().overlap { " (were inside each other)" } else { "" });
        Some(Chat::new(id, vec![a, b], Stage::Notice, timer))
    }

    /// Standing inside someone with nothing to say: one shuffles over.
    fn step_aside(&mut self, i: usize, other: usize, d: f32) {
        let (x, o) = (self.strip_px(i), self.strip_px(other));
        let far = (self.width_of(i) + self.width_of(other)) * 0.5 * 0.8;
        let dist = (far - d).max(30.0);
        let mut dir = if x < o {
            -1.0
        } else if x > o {
            1.0
        } else if self.walk.rand() < 0.5 {
            -1.0
        } else {
            1.0
        };
        if !(0.0..=self.strip_width()).contains(&(x + dir * dist)) {
            dir = -dir;
        }
        let step = dir * dist * 2.0 / self.view_w.max(1.0);
        log::debug!("chat: {i} steps aside from {other}");
        let wk = self.walk_of(i);
        wk.target = wk.x + step;
        wk.act = Act::Walk;
    }

    /// Someone free close to a chat: it may catch their eye. Joining starts
    /// the chat's time over; enough of them set off a race.
    fn try_join(&mut self, c: &mut Chat, n: usize) {
        let spots: Vec<f32> = c.members.iter().map(|&i| self.strip_px(i)).collect();
        let size = self.width_of(c.members[0]);
        let free: Vec<(usize, f32)> = (0..n)
            .filter(|&i| {
                let wk = self.walk_ref(i);
                grounded(wk) && wk.social.is_none() && wk.race.is_none() && !wk.circle && wk.energy >= tn().energy_join_need && chat_cool(wk) <= 0.0 && self.on_screen(i)
            })
            .map(|i| {
                let x = self.strip_px(i);
                (i, spots.iter().map(|p| (p - x).abs()).fold(f32::MAX, f32::min))
            })
            .filter(|f| f.1 < size * join_range(c.members.len()))
            .collect();
        let Some(&(i, _)) = free.iter().min_by(|a, b| a.1.total_cmp(&b.1)) else { return };
        let moving = c.stage == Stage::Stroll || self.walk_ref(i).act == Act::Walk;
        let chance = join_chance(c.members.len()) * if moving { tn().walk_join } else { 1.0 };
        if self.walk.rand() >= chance {
            return;
        }
        self.spend(i, tn().energy_join_cost);
        self.join(c, i, n);
    }

    /// Someone standing inside a member of a chat: a chat right there is
    /// hard to miss, so they join; if they've only just had one, they
    /// shuffle out of the way instead. True if it dealt with someone.
    fn overlap_join(&mut self, c: &mut Chat, n: usize, full: bool) -> bool {
        let mut hit: Option<(f32, usize, usize)> = None;
        for i in 0..n {
            let wk = self.walk_ref(i);
            if wk.act != Act::Idle || wk.social.is_some() || wk.race.is_some() || wk.circle || !self.on_screen(i) {
                continue;
            }
            let x = self.strip_px(i);
            for &m in &c.members {
                let d = (x - self.strip_px(m)).abs();
                let range = if self.walk_ref(i).dropped > 0.0 { tn().drop_range } else { tn().overlap };
                if d < (self.width_of(i) + self.width_of(m)) * 0.5 * range && hit.is_none_or(|h| d < h.0) {
                    hit = Some((d, i, m));
                }
            }
        }
        let Some((d, i, m)) = hit else { return false };
        // put right there by hand: no question about it
        if self.walk_ref(i).dropped > 0.0 {
            self.join(c, i, n);
        } else if !full && chat_cool(self.walk_ref(i)) <= tn().overlap_cool {
            if self.walk.rand() < tn().overlap_chance {
                self.join(c, i, n);
            }
        } else if self.walk.rand() < tn().step_chance {
            self.step_aside(i, m, d);
        }
        true
    }

    fn join(&mut self, c: &mut Chat, i: usize, n: usize) {
        c.quiet = 0.0;
        self.enlist(i, c.id);
        c.members.push(i);
        self.face(i, c.members[0]);
        self.walk_of(i).start_bob(tn().jump_height * 0.2);
        log::debug!("chat {}: {i} joins, now {}", c.id, c.members.len());
        match c.stage {
            Stage::Stroll => {
                self.catch_up(c, i);
                c.joined_walking = true;
            }
            // straight into the game
            Stage::Cursor => {
                if let Some(r) = self.walk_of(i).social.as_mut() {
                    r.chasing = true;
                }
                c.scores.push((i, 0.0));
            }
            _ => {
                // nobody waits for them: whatever's going on goes on
                self.arrange(c);
                if c.stage == Stage::Contest {
                    c.queue.push(i);
                } else if c.stage == Stage::Talk {
                    c.timer = self.talk_length() + Self::crowd_talk(c);
                }
            }
        }
        if c.members.len() >= race_at(n) && self.walk.rand() < tn().race_chance {
            self.race_first = Some(i);
            super::RACE_NOW.store(true, std::sync::atomic::Ordering::Relaxed);
            log::debug!("chat {}: {} together, race!", c.id, c.members.len());
        }
    }

    /// `i` looks towards `other`.
    fn face(&mut self, i: usize, other: usize) {
        let left = self.strip_px(other) < self.strip_px(i);
        if let Some(r) = self.walk_of(i).social.as_mut() {
            r.face_left = left;
        }
    }

    fn go(&mut self, i: usize, goal_px: f32, speed: f32) {
        if let Some(r) = self.walk_of(i).social.as_mut() {
            r.goal_px = goal_px;
            r.speed = speed;
            r.moving = true;
            r.happy = false;
        }
    }

    fn arrived(&self, i: usize) -> bool {
        let wk = self.walk_ref(i);
        grounded(wk) && wk.social.is_some_and(|s| !s.moving)
    }

    fn all_arrived(&self, c: &Chat) -> bool {
        c.members.iter().all(|&i| self.arrived(i))
    }

    fn goal_of(&self, i: usize) -> f32 {
        self.walk_ref(i).social.map_or(0.0, |s| s.goal_px)
    }

    fn strip_width(&self) -> f32 {
        crate::backend::compositor::current().strip().map_or(self.view_w, |s| s.width.max(self.view_w))
    }

    fn crowd_talk(c: &Chat) -> f32 {
        c.members.len().saturating_sub(2) as f32 * tn().crowd_talk
    }

    fn talk_length(&mut self) -> f32 {
        tn().talk_min + self.walk.rand() * tn().talk_rand
    }

    /// Joining a group on the move: head for the end of its row, a little
    /// faster than it walks.
    fn catch_up(&mut self, c: &Chat, i: usize) {
        let others: Vec<usize> = c.members.iter().copied().filter(|&m| m != i).collect();
        let goals: Vec<f32> = others.iter().map(|&m| self.goal_of(m)).collect();
        let mean = others.iter().map(|&m| self.strip_px(m)).sum::<f32>() / others.len() as f32;
        let gap = self.width_of(i) * 0.72;
        let goal = if self.strip_px(i) < mean {
            goals.iter().copied().fold(f32::MAX, f32::min) - gap
        } else {
            goals.iter().copied().fold(f32::MIN, f32::max) + gap
        };
        self.go(i, goal.clamp(0.0, self.strip_width()), 0.18);
    }

    /// Side by side in a row around their middle, in the order they stand.
    /// Whoever's in the happy pose stays in it.
    fn arrange(&mut self, c: &Chat) {
        let mut row: Vec<(usize, f32)> = c.members.iter().map(|&i| (i, self.strip_px(i))).collect();
        row.sort_by(|a, b| a.1.total_cmp(&b.1));
        let count = row.len() as f32;
        let mid = row.iter().map(|r| r.1).sum::<f32>() / count;
        let size = row.iter().map(|r| self.width_of(r.0)).sum::<f32>() / count;
        let gap = size * 0.72;
        for (k, &(i, _)) in row.iter().enumerate() {
            let happy = self.walk_ref(i).social.is_some_and(|r| r.happy);
            self.go(i, mid + (k as f32 - (count - 1.0) * 0.5) * gap, 0.16);
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.happy = happy;
            }
        }
    }

    /// Everyone looks at whoever is talking; the talker at the rest.
    fn look_at_talker(&mut self, c: &Chat) {
        let talker = self.goal_of(c.talker);
        let mean = c.members.iter().map(|&i| self.goal_of(i)).sum::<f32>() / c.members.len() as f32;
        for &i in &c.members {
            let own = self.goal_of(i);
            let at = if i == c.talker { mean } else { talker };
            if (at - own).abs() > 1.0 {
                if let Some(r) = self.walk_of(i).social.as_mut() {
                    r.face_left = at < own;
                }
            }
        }
    }

    fn start_talk(&mut self, c: &mut Chat, length: f32) {
        log::debug!("chat {}: {:?} talk {length:.1}s", c.id, c.members);
        c.talker = c.members[0];
        self.look_at_talker(c);
        c.stage = Stage::Talk;
        c.timer = length + Self::crowd_talk(c);
        c.turn = 0.0;
        c.bob = 0.0;
    }

    /// Off for a walk together, keeping their places in the row.
    fn stroll(&mut self, c: &mut Chat) {
        log::debug!("chat {}: stroll", c.id);
        let width = self.strip_width();
        let spots: Vec<f32> = c.members.iter().map(|&i| self.strip_px(i)).collect();
        let lo = spots.iter().copied().fold(f32::MAX, f32::min);
        let hi = spots.iter().copied().fold(f32::MIN, f32::max);
        let mut dist = tn().stroll_min + self.walk.rand() * tn().stroll_rand;
        let mut dir = if self.walk.rand() < 0.5 { -1.0 } else { 1.0 };
        let room = |d: f32| (if d < 0.0 { lo } else { width - hi } - tn().edge_px).max(0.0);
        if room(dir) < dist {
            if room(-dir) > room(dir) {
                dir = -dir;
            }
            dist = dist.min(room(dir));
        }
        for (k, &i) in c.members.clone().iter().enumerate() {
            self.go(i, spots[k] + dir * dist, 0.12);
        }
        c.strolls += 1;
        c.stage = Stage::Stroll;
    }

    fn spend(&mut self, i: usize, amount: f32) {
        let wk = self.walk_of(i);
        wk.energy = (wk.energy - amount).max(0.0);
    }

    /// Start a game this group hasn't played yet, if it has the energy.
    fn next_game(&mut self, c: &mut Chat) -> bool {
        let mean = c.members.iter().map(|&i| self.walk_ref(i).energy).sum::<f32>() / c.members.len() as f32;
        if mean < tn().energy_game_need {
            return false;
        }
        let len = c.members.len();
        let game = if !c.played.contains(&Game::Cursor) && self.walk.rand() < tn().cursor_game {
            Game::Cursor
        } else {
            let pool: Vec<Game> = [
                (Game::Contest, tn().game_min as usize),
                (Game::Long, tn().long_min as usize),
                (Game::Domino, tn().party_min as usize),
                (Game::Tug, tn().party_min as usize),
                (Game::Tag, tn().party_min as usize),
            ]
            .iter()
            .filter(|g| len >= g.1 && !c.played.contains(&g.0))
            .map(|g| g.0)
            .collect();
            if pool.is_empty() {
                return false;
            }
            pool[(self.walk.rand() * pool.len() as f32) as usize % pool.len()]
        };
        c.played.push(game);
        for &i in &c.members.clone() {
            self.spend(i, tn().energy_game_cost);
        }
        self.start_game(c, game);
        true
    }

    fn start_game(&mut self, c: &mut Chat, game: Game) {
        match game {
            Game::Contest => self.start_contest(c),
            Game::Cursor => self.start_cursor_game(c),
            Game::Long => self.start_long(c),
            Game::Domino => self.start_domino(c),
            Game::Tug => self.start_tug(c),
            Game::Tag => self.start_tag(c),
        }
    }

    /// A game asked for by hand: whoever is nearest the middle of the
    /// screen plays right now, whatever they were up to and however tired.
    fn force_game(&mut self, chats: &mut Vec<Chat>, name: &str, n: usize) {
        let t = tn();
        let (game, need) = match name {
            "contest" => (Game::Contest, t.game_min),
            "cursor" => (Game::Cursor, t.game_min),
            "long" => (Game::Long, t.long_min),
            "domino" => (Game::Domino, t.party_min),
            "tug" => (Game::Tug, t.party_min),
            "tag" => (Game::Tag, t.party_min),
            _ => return,
        };
        let need = need as usize;
        if self.race.is_some() || n < need {
            log::debug!("game {name}: can't start now ({n} characters, needs {need})");
            return;
        }
        for c in chats.drain(..) {
            self.finish(&c, n);
        }
        let middle = self.view[0] * self.view_w * 0.5 + self.view_w * 0.5;
        let mut free: Vec<(usize, f32)> = (0..n)
            .filter(|&i| !self.walk_ref(i).circle && grounded(self.walk_ref(i)))
            .map(|i| (i, (self.strip_px(i) - middle).abs()))
            .collect();
        free.sort_by(|a, b| a.1.total_cmp(&b.1));
        if free.len() < need {
            return;
        }
        let members: Vec<usize> = free.iter().take(need.max(FORCED_PLAYERS.min(n))).map(|f| f.0).collect();
        self.chat_id = self.chat_id.wrapping_add(1);
        let id = self.chat_id;
        for &i in &members {
            self.enlist(i, id);
        }
        log::debug!("chat {id}: {name} by hand, {members:?}");
        let mut c = Chat::new(id, members, Stage::Approach, 0.0);
        c.forced = Some(game);
        self.arrange(&c);
        chats.push(c);
    }

    fn steer(&mut self, i: usize, x: f32, speed: f32) {
        if let Some(r) = self.walk_of(i).social.as_mut() {
            r.goal_px = x;
            r.speed = speed;
            r.moving = true;
        }
    }

    /// Like `steer`, and looking where it's going even once it gets there.
    fn run_to(&mut self, i: usize, x: f32, speed: f32) {
        let here = self.strip_px(i);
        if let Some(r) = self.walk_of(i).social.as_mut() {
            if (x - here).abs() > 1.0 {
                r.face_left = x < here;
            }
        }
        self.steer(i, x, speed);
    }

    /// Down flat, falling towards `dir`.
    fn topple(&mut self, i: usize, dir: f32) {
        if let Some(r) = self.walk_of(i).social.as_mut() {
            r.stuck = true;
            r.rot_target = -dir * std::f32::consts::FRAC_PI_2;
            // lying on the ground, not half under it
            r.sink_target = -0.35;
            r.feet = true;
            r.happy = false;
        }
    }

    fn sorted_row(&self, c: &Chat) -> Vec<usize> {
        let mut row = c.members.clone();
        row.sort_by(|a, b| self.strip_px(*a).total_cmp(&self.strip_px(*b)));
        row
    }

    fn row_size(&self, c: &Chat) -> f32 {
        c.members.iter().map(|&i| self.width_of(i)).sum::<f32>() / c.members.len() as f32
    }

    /// Domino: the row goes down one after the other from one end; now and
    /// then the last one stays standing and is very pleased about it.
    fn start_domino(&mut self, c: &mut Chat) {
        log::debug!("chat {}: domino", c.id);
        let mut row = self.sorted_row(c);
        c.offset = if self.walk.rand() < 0.5 { 1.0 } else { -1.0 };
        if c.offset < 0.0 {
            row.reverse();
        }
        c.scores.clear();
        if self.walk.rand() < tn().domino_survive {
            if let Some(last) = row.pop() {
                c.scores.push((last, 1.0));
            }
        }
        c.queue = row;
        // shoulder to shoulder first, or nobody knocks anybody over
        self.arrange(c);
        c.wait = 0.0;
        c.step = 9;
        c.stage = Stage::Domino;
    }

    fn run_domino(&mut self, c: &mut Chat, dt: f32) {
        c.wait -= dt;
        c.queue.retain(|i| c.members.contains(i));
        if c.wait > 0.0 {
            return;
        }
        match c.step {
            9 => {
                if self.all_arrived(c) {
                    c.wait = 0.6;
                    c.step = 0;
                }
            }
            0 => {
                if c.queue.is_empty() {
                    c.wait = tn().domino_rest;
                    c.step = 1;
                } else {
                    let i = c.queue.remove(0);
                    self.topple(i, c.offset);
                    c.wait = tn().domino_gap;
                }
            }
            _ => self.end_game(c),
        }
    }

    /// Both ends of each rope in play, surface px (x0, y0, x1, y1), held at
    /// hand height by the one at the back of each side.
    pub(super) fn rope_ends(&self) -> Vec<[f32; 4]> {
        let n = self.extras.len() + 1;
        let hand = |i: usize| {
            let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
            ((r[0] + r[2]) * 0.5, r[1] + (r[3] - r[1]) * 0.62)
        };
        self.chats
            .iter()
            .filter(|c| c.stage == Stage::Tug && c.step == 1)
            .filter_map(|c| {
                let ends = |team: &Vec<usize>| -> Vec<(f32, f32)> {
                    team.iter().filter(|&&i| i < n && c.members.contains(&i)).map(|&i| hand(i)).collect()
                };
                let (left, right) = (ends(&c.teams.0), ends(&c.teams.1));
                let a = left.iter().copied().min_by(|p, q| p.0.total_cmp(&q.0))?;
                let b = right.iter().copied().max_by(|p, q| p.0.total_cmp(&q.0))?;
                Some([a.0, a.1, b.0, b.1])
            })
            .collect()
    }

    /// Tug of war: two sides facing each other, the whole line swaying until
    /// one side has pulled the other far enough, and those go down.
    fn start_tug(&mut self, c: &mut Chat) {
        log::debug!("chat {}: tug of war", c.id);
        let row = self.sorted_row(c);
        let half = row.len() / 2;
        let mid = row.iter().map(|&i| self.strip_px(i)).sum::<f32>() / row.len() as f32;
        let gap = self.row_size(c) * 0.72;
        c.base.clear();
        let left: Vec<usize> = row[..half].to_vec();
        let right: Vec<usize> = row[row.len() - half..].to_vec();
        for (k, &i) in left.iter().rev().enumerate() {
            c.base.push((i, mid - gap * (k as f32 + 0.6)));
        }
        for (k, &i) in right.iter().enumerate() {
            c.base.push((i, mid + gap * (k as f32 + 0.6)));
        }
        for &(i, x) in &c.base.clone() {
            self.go(i, x, 0.2);
        }
        c.teams = (left, right);
        c.offset = 0.0;
        c.pull = 0.0;
        c.pull_target = 0.0;
        c.wait = 0.0;
        c.step = 0;
        c.timer = tn().tug_time;
        c.scores.clear();
        c.regroup = true;
        c.stage = Stage::Tug;
    }

    fn run_tug(&mut self, c: &mut Chat, dt: f32) {
        c.wait -= dt;
        match c.step {
            0 => {
                if self.all_arrived(c) {
                    for (i, left) in c.teams.0.iter().map(|&i| (i, false)).chain(c.teams.1.iter().map(|&i| (i, true))).collect::<Vec<_>>() {
                        if let Some(r) = self.walk_of(i).social.as_mut() {
                            r.face_left = left;
                        }
                    }
                    c.timer = tn().tug_time;
                    c.step = 1;
                }
            }
            1 => {
                // every so often one side digs in harder; the rope follows
                // that smoothly, it never jumps
                if c.wait <= 0.0 {
                    c.wait = 0.5 + self.walk.rand() * 0.5;
                    let mut force = 0.0;
                    for _ in &c.teams.0 {
                        force -= self.walk.rand();
                    }
                    for _ in &c.teams.1 {
                        force += self.walk.rand();
                    }
                    c.pull_target = (force * tn().tug_pull).clamp(-tn().tug_speed, tn().tug_speed);
                }
                c.pull += (c.pull_target - c.pull) * (dt * 3.0).min(1.0);
                c.offset += c.pull * dt;
                for k in 0..c.base.len() {
                    let (i, x) = c.base[k];
                    if c.members.contains(&i) {
                        self.steer(i, x + c.offset, 0.6);
                    }
                }
                // leaning back, heaving in time, the side that's winning more so
                let heave = (c.timer * 5.0).sin() * 0.05;
                let winning = c.pull / tn().tug_speed * 0.08;
                let lean = [(c.teams.0.clone(), tn().tug_lean + heave - winning), (c.teams.1.clone(), -(tn().tug_lean + heave + winning))];
                for (team, amount) in lean {
                    for i in team {
                        if let Some(r) = self.walk_of(i).social.as_mut() {
                            r.lean = amount;
                        }
                    }
                }
                if c.offset.abs() >= tn().tug_win || c.timer <= 0.0 {
                    let dir = if c.offset >= 0.0 { 1.0 } else { -1.0 };
                    let (winners, losers) = if dir > 0.0 { (c.teams.1.clone(), c.teams.0.clone()) } else { (c.teams.0.clone(), c.teams.1.clone()) };
                    log::debug!("chat {}: tug won by {winners:?}", c.id);
                    c.scores = winners.iter().filter(|i| c.members.contains(i)).map(|&i| (i, 1.0)).collect();
                    for &i in winners.iter().chain(losers.iter()) {
                        if let Some(r) = self.walk_of(i).social.as_mut() {
                            r.lean = 0.0;
                        }
                    }
                    for i in losers.into_iter().filter(|i| c.members.contains(i)) {
                        // the rope gives and they go flying backwards
                        let distance = tn().tug_fling_min + self.walk.rand() * tn().tug_fling_rand;
                        let time = 0.7 + self.walk.rand() * 0.4;
                        self.fling(i, -dir, distance, time);
                    }
                    c.wait = 1.6;
                    c.step = 2;
                }
            }
            _ => {
                let airborne = c.members.iter().any(|&i| self.walk_ref(i).social.is_some_and(|s| s.flying));
                if c.wait <= 0.0 && !airborne {
                    self.end_game(c);
                }
            }
        }
    }

    /// Tag: one is "it" and runs after the nearest; a touch passes it on.
    /// Whoever spent the least time being "it" wins.
    fn start_tag(&mut self, c: &mut Chat) {
        log::debug!("chat {}: tag", c.id);
        let mid = c.members.iter().map(|&i| self.strip_px(i)).sum::<f32>() / c.members.len() as f32;
        let width = self.strip_width();
        let lo = (mid - tn().tag_arena).max(tn().edge_px.min(width * 0.5));
        let hi = (mid + tn().tag_arena).min((width - tn().edge_px).max(width * 0.5));
        c.spots = vec![lo, hi];
        c.it = c.members[(self.walk.rand() * c.members.len() as f32) as usize % c.members.len()];
        c.scores = c.members.iter().map(|&i| (i, 0.0)).collect();
        c.offset = 0.0;
        c.chase = c.it;
        c.wait = 1.0;
        c.timer = tn().tag_time_min + self.walk.rand() * tn().tag_time_rand;
        c.regroup = true;
        c.stage = Stage::Tag;
    }

    /// Bursts and lulls: a speed multiplier that eases towards a new value
    /// every second or so.
    fn tag_pace(&mut self, i: usize, dt: f32) -> f32 {
        let wk = self.walk_of(i);
        let (wait, goal) = (0.6 + wk.rand() * 0.9, 1.0 + (wk.rand() * 2.0 - 1.0) * tn().tag_pace);
        let Some(r) = wk.social.as_mut() else { return 1.0 };
        r.pace_wait -= dt;
        if r.pace_wait <= 0.0 {
            r.pace_wait = wait;
            r.pace_goal = goal;
        }
        r.pace += (r.pace_goal - r.pace) * (dt * 3.0).min(1.0);
        r.pace
    }

    fn run_tag(&mut self, c: &mut Chat, dt: f32) {
        if c.timer <= 0.0 {
            self.end_game(c);
            return;
        }
        if !c.members.contains(&c.it) {
            c.it = c.members[0];
        }
        c.wait -= dt;
        let (lo, hi) = (c.spots[0], c.spots[1]);
        let it_x = self.strip_px(c.it);
        let size = self.row_size(c);
        let mut nearest: Option<(usize, f32)> = None;
        for k in 0..c.members.len() {
            let i = c.members[k];
            if i == c.it {
                continue;
            }
            if let Some(s) = c.scores.iter_mut().find(|s| s.0 == i) {
                s.1 += dt;
            }
            let x = self.strip_px(i);
            let d = x - it_x;
            if !c.recent.contains(&i) && nearest.is_none_or(|n| d.abs() < n.1) {
                nearest = Some((i, d.abs()));
            }
            // runners stick to a decision for a moment, changing their mind every
            // frame just makes them shake on the spot
            let pace = self.tag_pace(i, dt);
            let wk = self.walk_of(i);
            let wait = 0.5 + wk.rand() * 0.5;
            let Some(r) = wk.social.as_mut() else { continue };
            r.speed = tn().tag_run * pace;
            r.hop_cool -= dt;
            if r.hop_cool > 0.0 {
                continue;
            }
            r.hop_cool = wait;
            // away from "it", and round the other way when cornered
            let away = if d >= 0.0 { 1.0 } else { -1.0 };
            let goal = if (away > 0.0 && hi - x < 90.0) || (away < 0.0 && x - lo < 90.0) { it_x - away * 500.0 } else { x + away * 500.0 };
            self.run_to(i, goal.clamp(lo, hi), tn().tag_run * pace);
        }
        let Some((n, d)) = nearest else { return };
        if c.wait > 0.0 {
            // just got caught: counts to one before running
            let x = it_x;
            self.steer(c.it, x, 0.1);
            return;
        }
        if d < size * 0.35 {
            log::debug!("chat {}: {n} is it", c.id);
            // no tagging straight back
            c.recent.push(c.it);
            let keep = c.members.len().saturating_sub(2).clamp(1, 2);
            let extra = c.recent.len().saturating_sub(keep);
            c.recent.drain(..extra);
            c.it = n;
            c.wait = 1.0;
            let wk = self.walk_of(n);
            if grounded(wk) {
                wk.start_bob(tn().jump_height * 0.2);
            }
            return;
        }
        // same for "it": one target at a time
        c.offset -= dt;
        if c.offset <= 0.0 || c.chase == c.it || c.recent.contains(&c.chase) || !c.members.contains(&c.chase) {
            c.chase = n;
            c.offset = 0.7;
        }
        let target = self.strip_px(c.chase);
        let pace = self.tag_pace(c.it, dt);
        self.run_to(c.it, target.clamp(lo, hi), tn().tag_it * pace);
    }

    /// Someone around sees a game going on: they come and stand by to watch,
    /// and join when it's over.
    fn try_watch(&mut self, c: &mut Chat, n: usize) {
        for &w in &c.watchers.clone() {
            if self.arrived(w) {
                self.face(w, c.talker);
                if self.walk.rand() < 0.12 {
                    self.walk_of(w).start_bob(tn().jump_height * 0.1);
                }
            }
        }
        // nobody new once the group would be too big anyway
        if c.members.len() + c.watchers.len() >= tn().max_group as usize {
            return;
        }
        let spots: Vec<f32> = c.members.iter().map(|&i| self.strip_px(i)).collect();
        let size = self.width_of(c.members[0]);
        let free: Vec<(usize, f32)> = (0..n)
            .filter(|&i| {
                let wk = self.walk_ref(i);
                grounded(wk) && wk.social.is_none() && wk.race.is_none() && !wk.circle && wk.energy >= tn().energy_watch_need && chat_cool(wk) <= tn().overlap_cool && self.on_screen(i)
            })
            .map(|i| {
                let x = self.strip_px(i);
                (i, spots.iter().map(|p| (p - x).abs()).fold(f32::MAX, f32::min))
            })
            .filter(|f| f.1 < size * tn().see_range)
            .collect();
        let Some(&(i, _)) = free.iter().min_by(|a, b| a.1.total_cmp(&b.1)) else { return };
        if self.walk.rand() >= tn().see_chance {
            return;
        }
        self.spend(i, tn().energy_watch_cost);
        self.enlist(i, c.id);
        c.watchers.push(i);
        log::debug!("chat {}: {i} comes to watch", c.id);
        // beside the row, not in the way
        let lo = c.members.iter().map(|&m| self.goal_of(m)).fold(f32::MAX, f32::min);
        let hi = c.members.iter().map(|&m| self.goal_of(m)).fold(f32::MIN, f32::max);
        let step = size * 0.85 * c.watchers.len() as f32;
        let goal = if self.strip_px(i) < (lo + hi) * 0.5 { lo - step } else { hi + step };
        self.go(i, goal.clamp(0.0, self.strip_width()), 0.18);
    }

    /// The game's over: some of those watching come in, the rest move on.
    fn take_in_watchers(&mut self, c: &mut Chat) {
        for i in std::mem::take(&mut c.watchers) {
            if self.walk.rand() < tn().watch_join {
                log::debug!("chat {}: {i} joins after the game", c.id);
                c.members.push(i);
                c.regroup = true;
                c.quiet = 0.0;
            } else {
                self.leave(i, c.id);
            }
        }
    }

    /// A big crowd doesn't have to stay one: half of it goes off to do its
    /// own thing as a separate group.
    fn maybe_split(&mut self, c: &mut Chat) {
        if (c.members.len() as f32) < tn().split_min || self.walk.rand() >= tn().split_chance {
            return;
        }
        let row = self.sorted_row(c);
        let (left, right) = row.split_at(row.len() / 2);
        self.chat_id = self.chat_id.wrapping_add(1);
        let id = self.chat_id;
        let mut other = Chat::new(id, right.to_vec(), Stage::Approach, 0.0);
        other.games = c.games;
        other.played = c.played.clone();
        log::debug!("chat {}: splits, {:?} go off as chat {id}", c.id, other.members);
        for &i in &other.members {
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.chat = id;
            }
        }
        c.members = left.to_vec();
        c.talker = c.members[0];
        c.regroup = true;
        self.arrange(&other);
        self.arrange(c);
        // a bit of room between the two
        let size = self.row_size(c);
        for (members, away) in [(c.members.clone(), -1.0), (other.members.clone(), 1.0)] {
            for i in members {
                let x = self.goal_of(i);
                self.steer(i, x + away * size * 1.5, 0.16);
            }
        }
        self.new_chats.push(other);
    }

    fn start_contest(&mut self, c: &mut Chat) {
        log::debug!("chat {}: jumping game", c.id);
        let mut queue = c.members.clone();
        for k in (1..queue.len()).rev() {
            let j = (self.walk.rand() * (k + 1) as f32) as usize % (k + 1);
            queue.swap(k, j);
        }
        c.queue = queue;
        c.wait = 0.0;
        c.scores.clear();
        c.step = 0;
        c.stage = Stage::Contest;
    }

    /// Long jump: one referee. An odd group plays in pairs, one throwing the
    /// other; an even one each on their own with a run-up.
    fn start_long(&mut self, c: &mut Chat) {
        let mut all = c.members.clone();
        for k in (1..all.len()).rev() {
            let j = (self.walk.rand() * (k + 1) as f32) as usize % (k + 1);
            all.swap(k, j);
        }
        let referee = all.remove(0);
        let pairs = c.members.len() % 2 == 1;
        let turns: Vec<Turn> = if pairs {
            all.chunks_exact(2).map(|p| Turn { flyer: p[0], thrower: Some(p[1]) }).collect()
        } else {
            all.iter().map(|&i| Turn { flyer: i, thrower: None }).collect()
        };
        log::debug!("chat {}: long jump, referee {referee}, {} turns ({})", c.id, turns.len(), if pairs { "pairs" } else { "singles" });
        let width = self.strip_width();
        let mean = c.members.iter().map(|&i| self.strip_px(i)).sum::<f32>() / c.members.len() as f32;
        // plenty of ground ahead
        let dir = if mean < width * 0.5 { 1.0 } else { -1.0 };
        let line = (mean - dir * 150.0).clamp(tn().edge_px.min(width * 0.5), (width - tn().edge_px).max(width * 0.5));
        let size = self.width_of(c.members[0]);
        let mut back = 1.0;
        for t in &turns {
            self.go(t.flyer, line - dir * size * 0.85 * back, 0.16);
            back += 1.0;
            if let Some(th) = t.thrower {
                self.go(th, line - dir * size * 0.85 * back, 0.16);
                back += 1.0;
            }
        }
        self.go(referee, line + dir * size, 0.16);
        c.long = Some(Long { referee, line, dir, ready: false, turns, next: 0, gap: 0.0, running: Vec::new(), landed: Vec::new(), judging: None, thrown: 0.0 });
        c.scores.clear();
        c.regroup = true;
        c.timer = tn().long_time;
        c.stage = Stage::Long;
    }

    /// Off they go, very far, tumbling.
    fn launch(&mut self, i: usize, dir: f32) {
        let wk = self.walk_of(i);
        let distance = tn().long_dist_min + wk.rand() * tn().long_dist_rand;
        let time = tn().long_air_min + wk.rand() * tn().long_air_rand;
        self.fling(i, dir, distance, time);
    }

    /// Through the air, tumbling, to end up planted in the ground.
    fn fling(&mut self, i: usize, dir: f32, distance: f32, time: f32) {
        let w = self.view_w.max(1.0);
        let from = self.strip_px(i);
        let wk = self.walk_of(i);
        let spin = dir * (5.0 + wk.rand() * 9.0);
        wk.vx = dir * (distance * 2.0 / w) / time;
        wk.vy = 20.0 * time;
        wk.air = 0.1;
        wk.jump = 0.0;
        wk.held_prev = None;
        wk.act = Act::Fall;
        if let Some(r) = wk.social.as_mut() {
            r.flying = true;
            r.launch_px = from;
            r.happy = true;
            r.spin = spin;
            r.moving = false;
            r.bump_cool = 0.2;
        }
    }

    /// Those in the air knock into each other and into whoever is already
    /// stuck in the ground.
    fn long_collisions(&mut self, c: &Chat, dt: f32) {
        let mut flyers = Vec::new();
        let mut planted = Vec::new();
        for &i in &c.members {
            let x = self.strip_px(i);
            let width = self.width_of(i);
            let wk = self.walk_of(i);
            let air = wk.air;
            let Some(r) = wk.social.as_mut() else { continue };
            r.bump_cool = (r.bump_cool - dt).max(0.0);
            if r.flying {
                flyers.push((i, x, air, width, r.bump_cool));
            } else if r.stuck {
                planted.push((x, width));
            }
        }
        for (k, a) in flyers.iter().enumerate() {
            for b in &flyers[k + 1..] {
                if a.4 > 0.0 || b.4 > 0.0 || (a.1 - b.1).abs() > (a.3 + b.3) * 0.3 || (a.2 - b.2).abs() > 1.6 {
                    continue;
                }
                // bang: they swap speeds and both get knocked up
                let (va, vb) = (self.walk_ref(a.0).vx, self.walk_ref(b.0).vx);
                for (i, v) in [(a.0, vb), (b.0, va)] {
                    let wk = self.walk_of(i);
                    wk.vx = v * 0.85;
                    wk.vy = wk.vy.max(6.0) + 3.0;
                    if let Some(r) = wk.social.as_mut() {
                        r.spin = -r.spin * 1.2;
                        r.bump_cool = 0.5;
                    }
                }
            }
        }
        for f in &flyers {
            if f.4 > 0.0 || f.2 > 1.6 {
                continue;
            }
            if planted.iter().any(|p| (f.1 - p.0).abs() < (f.3 + p.1) * 0.3) {
                let wk = self.walk_of(f.0);
                wk.vx = -wk.vx * 0.6;
                wk.vy = wk.vy.max(10.0);
                if let Some(r) = wk.social.as_mut() {
                    r.spin = -r.spin;
                    r.bump_cool = 0.4;
                }
            }
        }
    }

    /// One frame of the long jump; false when it's over.
    fn run_long(&mut self, c: &mut Chat, dt: f32) {
        let Some(mut l) = c.long.take() else { return };
        if c.timer <= 0.0 || !c.members.contains(&l.referee) {
            self.end_game(c);
            return;
        }
        if !l.ready {
            l.ready = self.all_arrived(c);
            c.long = Some(l);
            return;
        }
        // next one sets off, a little after the last so they meet in the air
        l.gap -= dt;
        // a pair has the line to itself until it has thrown
        let pairs = l.turns.first().is_some_and(|t| t.thrower.is_some());
        while l.gap <= 0.0 && l.next < l.turns.len() && (!pairs || l.running.is_empty()) {
            let t = l.turns[l.next];
            l.next += 1;
            l.gap = tn().long_gap_min + self.walk.rand() * tn().long_gap_rand;
            if !c.members.contains(&t.flyer) {
                continue;
            }
            let thrower = t.thrower.filter(|th| c.members.contains(th));
            // everyone goes from the same line, or it isn't fair: alone at a
            // sprint (front of the queue first, so nobody's in the way), a pair
            // walking up, the thrower right behind
            match thrower {
                None => self.go(t.flyer, l.line, 0.7),
                Some(th) => {
                    let size = self.width_of(th);
                    self.go(t.flyer, l.line, 0.3);
                    self.go(th, l.line - l.dir * size * 0.85, 0.3);
                }
            }
            l.running.push(Turn { flyer: t.flyer, thrower });
        }
        for t in l.running.clone() {
            let gone = !c.members.contains(&t.flyer);
            if gone || (self.arrived(t.flyer) && t.thrower.is_none_or(|th| !c.members.contains(&th) || self.arrived(th))) {
                if !gone {
                    if let Some(th) = t.thrower.filter(|th| c.members.contains(th)) {
                        // thrown: clear the line for the next pair
                        let size = self.width_of(th);
                        self.go(th, l.line + l.dir * size * (1.9 + 0.85 * l.thrown), 0.3);
                        l.thrown += 1.0;
                        self.walk_of(th).start_bob(tn().jump_height * 0.2);
                    }
                    self.launch(t.flyer, l.dir);
                }
                l.running.retain(|r| r.flyer != t.flyer);
            }
        }
        self.long_collisions(c, dt);
        for &i in &c.members {
            if let Some(r) = self.walk_of(i).social.as_mut() {
                if r.stuck && !r.queued {
                    r.queued = true;
                    l.landed.push(i);
                }
            }
        }
        // the referee goes over to each one that came down
        l.landed.retain(|i| c.members.contains(i));
        match l.judging {
            Some((i, _)) if !c.members.contains(&i) => l.judging = None,
            Some((i, wait)) => {
                if self.arrived(l.referee) {
                    self.face(l.referee, i);
                    let wait = wait - dt;
                    if wait <= 0.0 {
                        let from = self.walk_ref(i).social.map_or(l.line, |s| s.launch_px);
                        let distance = ((self.strip_px(i) - from) * l.dir).max(1.0);
                        c.scores.push((i, distance));
                        log::debug!("chat {}: {i} jumped {distance:.0}", c.id);
                        if let Some(r) = self.walk_of(i).social.as_mut() {
                            r.stuck = false;
                        }
                        self.walk_of(l.referee).start_bob(tn().jump_height * 0.15);
                        l.judging = None;
                    } else {
                        l.judging = Some((i, wait));
                    }
                }
            }
            None => {
                if !l.landed.is_empty() {
                    let i = l.landed.remove(0);
                    let x = self.strip_px(i);
                    let size = self.width_of(i);
                    self.go(l.referee, x - l.dir * size * 0.7, 0.28);
                    l.judging = Some((i, tn().long_judge));
                }
            }
        }
        let airborne = c.members.iter().any(|&i| self.walk_ref(i).social.is_some_and(|s| s.flying || s.stuck));
        if l.next >= l.turns.len() && l.running.is_empty() && l.landed.is_empty() && l.judging.is_none() && !airborne {
            self.end_game(c);
            return;
        }
        c.long = Some(l);
    }

    fn start_cursor_game(&mut self, c: &mut Chat) {
        log::debug!("chat {}: cursor game", c.id);
        for &i in &c.members.clone() {
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.chasing = true;
                r.happy = false;
            }
        }
        c.scores = c.members.iter().map(|&i| (i, 0.0)).collect();
        c.timer = tn().cursor_time_min + self.walk.rand() * tn().cursor_time_rand;
        c.stage = Stage::Cursor;
    }

    /// The game's over: the best one hops about, if anyone scored at all.
    fn end_game(&mut self, c: &mut Chat) {
        for &i in &c.members.clone() {
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.chasing = false;
                r.happy = false;
                r.flying = false;
                r.stuck = false;
                r.lean = 0.0;
            }
        }
        let best = c.scores.iter().filter(|s| s.1 > 0.0 && c.members.contains(&s.0)).max_by(|a, b| a.1.total_cmp(&b.1));
        c.winner = best.map(|s| s.0);
        log::debug!("chat {}: game over, {:?} won of {:?}", c.id, c.winner, c.scores);
        if let Some(w) = c.winner {
            c.talker = w;
            self.look_at_talker(c);
            if let Some(r) = self.walk_of(w).social.as_mut() {
                r.happy = true;
            }
            c.timer = tn().cheer_time;
            c.bob = 0.0;
            c.stage = Stage::Cheer;
        } else {
            self.talk_it_over(c);
        }
    }

    /// A game's not over till it's been talked about.
    fn talk_it_over(&mut self, c: &mut Chat) {
        self.take_in_watchers(c);
        self.maybe_split(c);
        if std::mem::take(&mut c.regroup) {
            self.arrange(c);
            c.stage = Stage::Approach;
            return;
        }
        let length = tn().after_game_min + self.walk.rand() * tn().after_game_rand;
        self.start_talk(c, length);
    }

    /// And their own ways, away from the middle of the group.
    fn part(&mut self, c: &mut Chat) {
        log::debug!("chat {}: part", c.id);
        let width = self.strip_width();
        let mean = c.members.iter().map(|&i| self.strip_px(i)).sum::<f32>() / c.members.len() as f32;
        for &i in &c.members.clone() {
            let x = self.strip_px(i);
            let dist = 300.0 + self.walk.rand() * 500.0;
            let away = if (x - mean).abs() < 1.0 {
                if self.walk.rand() < 0.5 { -1.0 } else { 1.0 }
            } else {
                (x - mean).signum()
            };
            self.go(i, (x + away * dist).clamp(tn().edge_px.min(width * 0.5), (width - tn().edge_px).max(width * 0.5)), 0.12);
        }
        c.stage = Stage::Part;
    }

    /// Run one chat for a frame; false when it's over.
    fn advance(&mut self, c: &mut Chat, dt: f32, mouse: [f32; 2]) -> bool {
        c.timer -= dt;
        match c.stage {
            Stage::Notice => {
                // the other one looks back a moment later
                if c.timer < 0.6 {
                    self.face(c.members[1], c.members[0]);
                }
                if c.timer <= 0.0 {
                    // "oh, hi": a little hop each, then side by side
                    self.arrange(c);
                    c.stage = Stage::Approach;
                    for &i in &c.members {
                        self.walk_of(i).start_bob(tn().jump_height * 0.2);
                    }
                }
            }
            Stage::Approach => {
                if self.all_arrived(c) {
                    if let Some(game) = c.forced.take() {
                        c.played.push(game);
                        c.games += 1;
                        self.start_game(c, game);
                        return true;
                    }
                    // some are just a quick hello
                    let length = if c.members.len() == 2 && self.walk.rand() < tn().short_talk_chance {
                        tn().short_talk_min + self.walk.rand() * tn().short_talk_rand
                    } else {
                        self.talk_length()
                    };
                    self.start_talk(c, length);
                }
            }
            Stage::Talk => {
                c.quiet += dt;
                let may_play = c.members.len() >= tn().game_min as usize
                    && c.quiet >= tn().game_after
                    && (c.games == 0 || (c.members.len() >= tn().crowd_games as usize && (c.games as f32) < tn().max_games));
                if may_play {
                    // not every chat turns into a game; ask again a good while later
                    if self.walk.rand() >= tn().game_chance {
                        c.quiet = -2.0 * tn().game_after;
                    } else if self.next_game(c) {
                        c.games += 1;
                        c.quiet = 0.0;
                        return true;
                    }
                }
                if c.timer <= 0.0 {
                    if (c.strolls as f32) < tn().max_strolls && self.walk.rand() < tn().stroll_chance {
                        self.stroll(c);
                    } else {
                        self.part(c);
                    }
                    return true;
                }
                c.turn -= dt;
                if c.turn <= 0.0 {
                    // next one in the row who's in place (a newcomer may still be walking)
                    let at = c.members.iter().position(|&i| i == c.talker).map_or(0, |k| k + 1);
                    let count = c.members.len();
                    if let Some(next) = (0..count).map(|k| c.members[(at + k) % count]).find(|&i| self.arrived(i)) {
                        c.talker = next;
                    }
                    c.turn = 1.0 + self.walk.rand() * 1.6;
                    self.look_at_talker(c);
                    let roll = self.walk.rand();
                    let laugh = roll < 0.15;
                    for &i in &c.members {
                        let talking = i == c.talker;
                        let in_place = self.arrived(i);
                        let wk = self.walk_of(i);
                        if let Some(r) = wk.social.as_mut() {
                            r.happy = talking || laugh;
                        }
                        if laugh && in_place {
                            wk.start_bob(tn().jump_height * 0.15);
                        }
                    }
                    if !laugh && roll < 0.45 {
                        self.walk_of(c.talker).gesture = true;
                    }
                }
                // nodding along while talking
                c.bob -= dt;
                if c.bob <= 0.0 {
                    let amp = tn().jump_height * (0.05 + self.walk.rand() * 0.04);
                    c.bob = 0.5 + self.walk.rand() * 0.6;
                    if self.arrived(c.talker) {
                        self.walk_of(c.talker).start_bob(amp);
                    }
                }
            }
            Stage::Stroll => {
                if self.all_arrived(c) {
                    let length = if std::mem::take(&mut c.joined_walking) {
                        self.talk_length()
                    } else {
                        3.0 + self.walk.rand() * 2.0
                    };
                    self.start_talk(c, length);
                }
            }
            Stage::Contest => {
                c.wait -= dt;
                // the one whose turn it was got picked up or wandered off
                while c.queue.first().is_some_and(|i| !c.members.contains(i)) {
                    c.queue.remove(0);
                    c.step = 0;
                }
                let Some(&i) = c.queue.first() else {
                    self.end_game(c);
                    return true;
                };
                match c.step {
                    // its turn, right where it stands: everyone looks
                    0 if c.wait <= 0.0 => {
                        c.talker = i;
                        self.look_at_talker(c);
                        if let Some(r) = self.walk_of(i).social.as_mut() {
                            r.happy = true;
                        }
                        c.wait = 0.35;
                        c.step = 1;
                    }
                    // up, as high as it can this time
                    1 if c.wait <= 0.0 && grounded(self.walk_ref(i)) => {
                        let height = tn().contest_min + self.walk.rand() * tn().contest_rand;
                        self.walk_of(i).start_jump(tn().jump_height * height);
                        c.scores.push((i, height));
                        c.step = 2;
                    }
                    // landed: the next one
                    2 if grounded(self.walk_ref(i)) => {
                        if let Some(r) = self.walk_of(i).social.as_mut() {
                            r.happy = false;
                        }
                        c.queue.remove(0);
                        c.wait = 0.4;
                        c.step = 0;
                    }
                    _ => {}
                }
            }
            Stage::Cursor => {
                if c.timer <= 0.0 {
                    self.end_game(c);
                    return true;
                }
                // long enough on the cursor: that's the game
                if c.scores.iter().any(|s| s.1 >= tn().cursor_win) {
                    self.end_game(c);
                    return true;
                }
                let cursor = mouse[0] + self.view[0] * self.view_w * 0.5;
                let count = c.members.len() as f32;
                for k in 0..c.members.len() {
                    let i = c.members[k];
                    let x = self.strip_px(i);
                    let wk = self.walk_of(i);
                    // seconds hanging off the cursor are the score
                    if wk.act == Act::Held && wk.hang > 0.0 {
                        if let Some(s) = c.scores.iter_mut().find(|s| s.0 == i) {
                            s.1 += dt;
                        }
                    }
                    let (reach, on_ground) = (wk.reach, grounded(wk));
                    let roll = wk.rand();
                    let Some(r) = wk.social.as_mut() else { continue };
                    r.hop_cool = (r.hop_cool - dt).max(0.0);
                    r.goal_px = cursor + (k as f32 - (count - 1.0) * 0.5) * 30.0;
                    r.speed = tn().cursor_speed;
                    r.moving = true;
                    r.face_left = cursor < x;
                    let jump = on_ground && r.hop_cool <= 0.0 && (x - cursor).abs() < tn().cursor_reach && reach > 0.0;
                    if jump {
                        r.hop_cool = 0.6 + roll * 0.8;
                        wk.start_jump(reach.max(tn().jump_height));
                        wk.catch_jump = true;
                    }
                }
            }
            Stage::Long => self.run_long(c, dt),
            Stage::Domino => self.run_domino(c, dt),
            Stage::Tug => self.run_tug(c, dt),
            Stage::Tag => self.run_tag(c, dt),
            Stage::Cheer => {
                if c.timer <= 0.0 {
                    self.talk_it_over(c);
                    return true;
                }
                if let Some(w) = c.winner.filter(|w| c.members.contains(w)) {
                    let wk = self.walk_of(w);
                    if grounded(wk) {
                        wk.start_jump(tn().jump_height * 0.7);
                    }
                    // the others cheer along, one little bob at a time
                    c.bob -= dt;
                    if c.bob <= 0.0 {
                        c.bob = 0.3 + self.walk.rand() * 0.4;
                        let others: Vec<usize> = c.members.iter().copied().filter(|&i| i != w).collect();
                        if !others.is_empty() {
                            let i = others[(self.walk.rand() * others.len() as f32) as usize % others.len()];
                            let wk = self.walk_of(i);
                            if let Some(r) = wk.social.as_mut() {
                                r.happy = true;
                            }
                            if grounded(wk) {
                                wk.start_bob(tn().jump_height * 0.1);
                            }
                        }
                    }
                }
            }
            Stage::Part => return !self.all_arrived(c),
        }
        true
    }
}
