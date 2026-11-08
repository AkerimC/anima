//! Characters taking notice of each other: two standing close look over,
//! step side by side, chat for a while, maybe stroll together, then go their
//! own ways. Others passing by can join in, and a chat that grows to five
//! turns into a race. Only standing ones start one: walking through each
//! other is fine.

use super::CharacterOverlay;
use super::walk::{Act, JUMP_AMP, WalkState};

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
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Notice,
    Approach,
    Talk,
    Stroll,
    Part,
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
}

/// How often pairs are looked for, seconds.
const LOOK_EVERY: f32 = 0.4;
/// Chance per look that two standing close start a chat, or that someone
/// nearby joins one.
const NEAR_CHANCE: f32 = 0.03;
/// Same, when they stand inside each other: that shouldn't last.
const OVERLAP_CHANCE: f32 = 0.45;
/// Closer than this counts as inside each other, in character widths (a
/// chat's row stands 0.72 apart).
const OVERLAP: f32 = 0.7;
/// Inside each other but not up for a chat: chance per look to step aside.
const STEP_CHANCE: f32 = 0.2;
/// Pause after a chat, seconds: base and random part.
const COOL: (f32, f32) = (40.0, 50.0);
/// Standing inside someone overrides the pause once this little of it is left.
const OVERLAP_COOL: f32 = 45.0;
/// How far a chat catches someone's eye, in character widths.
const JOIN_RANGE: f32 = 2.2;
/// A chat this big turns into a race.
const RACE_AT: usize = 5;
/// Strolling together: this far at most, logical px (base and random part),
/// and this many walks per chat.
const STROLL: (f32, f32) = (700.0, 800.0);
const MAX_STROLLS: u8 = 2;

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
        self.walk.social = None;
        for e in &mut self.extras {
            e.walk.social = None;
        }
    }

    pub(super) fn run_social(&mut self, dt: f32) {
        let n = self.extras.len() + 1;
        for i in 0..n {
            let wk = self.walk_of(i);
            wk.social_cool = (wk.social_cool - dt).max(0.0);
        }
        let music = self.vibe.on;
        let mut chats = std::mem::take(&mut self.chats);
        chats.retain_mut(|c| {
            let keep = !music && self.prune(c, n) && self.advance(c, dt);
            if !keep {
                self.finish(c, n);
            }
            keep
        });

        self.social_tick -= dt;
        if self.social_tick <= 0.0 {
            self.social_tick = LOOK_EVERY;
            if self.race.is_none() && !music {
                for c in chats.iter_mut().filter(|c| c.stage == Stage::Talk && c.members.len() < RACE_AT) {
                    self.try_join(c, n);
                }
                let room = chats.len() < (n / 4).max(1);
                chats.extend(self.find_pair(n, room));
            }
        }
        self.chats = chats;
    }

    /// Drop members who were picked up, called away and so on; false when
    /// too few are left to go on.
    fn prune(&mut self, c: &mut Chat, n: usize) -> bool {
        let id = c.id;
        let mut left = Vec::new();
        c.members.retain(|&i| {
            let stays = i < n && {
                let wk = self.walk_ref(i);
                wk.social.is_some_and(|s| s.chat == id) && !wk.circle && !matches!(wk.act, Act::Held | Act::Fall | Act::Pat | Act::Chase)
            };
            if !stays {
                left.push(i);
            }
            stays
        });
        for i in left.into_iter().filter(|&i| i < n) {
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
            wk.social_cool = COOL.0 + wk.rand() * COOL.1;
        }
    }

    fn finish(&mut self, c: &Chat, n: usize) {
        for &i in c.members.iter().filter(|&&i| i < n) {
            self.leave(i, c.id);
        }
    }

    fn width_of(&self, i: usize) -> f32 {
        let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        (r[2] - r[0]).max(1.0)
    }

    fn on_screen(&self, i: usize) -> bool {
        let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        r[2] >= 0.0 && r[0] <= self.view_w
    }

    fn enlist(&mut self, i: usize, id: u32) {
        let wk = self.walk_of(i);
        let face_left = wk.left;
        wk.social = Some(Social { chat: id, goal_px: 0.0, goal: 0.0, speed: 0.0, moving: false, face_left, happy: false });
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
            .map(|i| (i, self.strip_px(i), self.width_of(i), self.walk_ref(i).social_cool))
            .collect();
        let mut overlap: Option<(f32, usize, usize)> = None;
        let mut near: Option<(f32, usize, usize)> = None;
        let mut crowded: Option<(f32, usize, usize)> = None;
        for (k, p) in free.iter().enumerate() {
            for q in &free[k + 1..] {
                let d = (p.1 - q.1).abs();
                let size = (p.2 + q.2) * 0.5;
                let slot = if d < size * OVERLAP {
                    if p.3 <= OVERLAP_COOL && q.3 <= OVERLAP_COOL { &mut overlap } else { &mut crowded }
                } else if d < size * 1.5 && p.3 <= 0.0 && q.3 <= 0.0 {
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
            (Some(o), _) if roll < OVERLAP_CHANCE => o,
            (_, Some(o)) if room && roll < NEAR_CHANCE => o,
            _ => {
                if let Some((d, a, b)) = crowded {
                    if self.walk.rand() < STEP_CHANCE {
                        self.step_aside(a, b, d);
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
        log::debug!("chat {id}: {a} and {b}{}", if pair.0 < self.width_of(a) * OVERLAP { " (were inside each other)" } else { "" });
        Some(Chat { id, members: vec![a, b], stage: Stage::Notice, timer, talker: a, turn: 0.0, bob: 0.0, strolls: 0 })
    }

    /// Standing inside someone with nothing to say: one shuffles over.
    fn step_aside(&mut self, a: usize, b: usize, d: f32) {
        let (i, other) = if self.walk.rand() < 0.5 { (a, b) } else { (b, a) };
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
    /// the chat's time over; the fifth one sets off a race.
    fn try_join(&mut self, c: &mut Chat, n: usize) {
        let spots: Vec<f32> = c.members.iter().map(|&i| self.strip_px(i)).collect();
        let size = self.width_of(c.members[0]);
        let free: Vec<(usize, f32)> = (0..n)
            .filter(|&i| {
                let wk = self.walk_ref(i);
                grounded(wk) && wk.social.is_none() && wk.race.is_none() && !wk.circle && wk.social_cool <= 0.0 && self.on_screen(i)
            })
            .map(|i| {
                let x = self.strip_px(i);
                (i, spots.iter().map(|p| (p - x).abs()).fold(f32::MAX, f32::min))
            })
            .filter(|f| f.1 < size * JOIN_RANGE)
            .collect();
        let Some(&(i, _)) = free.iter().min_by(|a, b| a.1.total_cmp(&b.1)) else { return };
        if self.walk.rand() >= NEAR_CHANCE {
            return;
        }
        self.enlist(i, c.id);
        c.members.push(i);
        self.face(i, c.members[0]);
        self.walk_of(i).start_bob(JUMP_AMP * 0.2);
        log::debug!("chat {}: {i} joins, now {}", c.id, c.members.len());
        self.arrange(c);
        if c.members.len() >= RACE_AT {
            self.race_first = Some(i);
            super::RACE_NOW.store(true, std::sync::atomic::Ordering::Relaxed);
            log::debug!("chat {}: five together, race!", c.id);
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

    /// Side by side in a row around their middle, in the order they stand.
    fn arrange(&mut self, c: &mut Chat) {
        let mut row: Vec<(usize, f32)> = c.members.iter().map(|&i| (i, self.strip_px(i))).collect();
        row.sort_by(|a, b| a.1.total_cmp(&b.1));
        let count = row.len() as f32;
        let mid = row.iter().map(|r| r.1).sum::<f32>() / count;
        let size = row.iter().map(|r| self.width_of(r.0)).sum::<f32>() / count;
        let gap = size * 0.72;
        for (k, &(i, _)) in row.iter().enumerate() {
            self.go(i, mid + (k as f32 - (count - 1.0) * 0.5) * gap, 0.16);
        }
        c.stage = Stage::Approach;
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
        c.timer = length;
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
        let mut dist = STROLL.0 + self.walk.rand() * STROLL.1;
        let mut dir = if self.walk.rand() < 0.5 { -1.0 } else { 1.0 };
        let room = |d: f32| if d < 0.0 { lo } else { (width - hi).max(0.0) };
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
            self.go(i, (x + away * dist).clamp(0.0, width), 0.12);
        }
        c.stage = Stage::Part;
    }

    /// Run one chat for a frame; false when it's over.
    fn advance(&mut self, c: &mut Chat, dt: f32) -> bool {
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
                    for &i in &c.members {
                        self.walk_of(i).start_bob(JUMP_AMP * 0.2);
                    }
                }
            }
            Stage::Approach => {
                if self.all_arrived(c) {
                    // some are just a quick hello
                    let length = if self.walk.rand() < 0.3 {
                        4.0 + self.walk.rand() * 3.0
                    } else {
                        8.0 + self.walk.rand() * 6.0
                    };
                    self.start_talk(c, length);
                }
            }
            Stage::Talk => {
                if c.timer <= 0.0 {
                    if c.strolls < MAX_STROLLS && self.walk.rand() < 0.3 {
                        self.stroll(c);
                    } else {
                        self.part(c);
                    }
                    return true;
                }
                c.turn -= dt;
                if c.turn <= 0.0 {
                    let at = c.members.iter().position(|&i| i == c.talker).map_or(0, |k| k + 1);
                    c.talker = c.members[at % c.members.len()];
                    c.turn = 1.0 + self.walk.rand() * 1.6;
                    self.look_at_talker(c);
                    let roll = self.walk.rand();
                    let laugh = roll < 0.15;
                    for &i in &c.members {
                        let talking = i == c.talker;
                        let wk = self.walk_of(i);
                        if let Some(r) = wk.social.as_mut() {
                            r.happy = talking || laugh;
                        }
                        if laugh && grounded(wk) {
                            wk.start_bob(JUMP_AMP * 0.15);
                        }
                    }
                    if !laugh && roll < 0.45 {
                        self.walk_of(c.talker).gesture = true;
                    }
                }
                // nodding along while talking
                c.bob -= dt;
                if c.bob <= 0.0 {
                    let amp = JUMP_AMP * (0.05 + self.walk.rand() * 0.04);
                    c.bob = 0.5 + self.walk.rand() * 0.6;
                    let wk = self.walk_of(c.talker);
                    if grounded(wk) {
                        wk.start_bob(amp);
                    }
                }
            }
            Stage::Stroll => {
                if self.all_arrived(c) {
                    let length = 3.0 + self.walk.rand() * 2.0;
                    self.start_talk(c, length);
                }
            }
            Stage::Part => return !self.all_arrived(c),
        }
        true
    }
}
