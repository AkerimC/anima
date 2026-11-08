//! Races: now and then one character wants to race, the others catch the
//! mood one by one, gather at one end of a big window and set off together.

use crate::render::tuning::get as tn;
use super::CharacterOverlay;
use super::walk::{Act, WalkState};

/// Set over IPC (`anima race`): start a race now.
pub static RACE_NOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Leg {
    // on the way to the gathering spot
    ToStart,
    // there, wandering about until everyone has come
    Mill,
    // everyone's here: bunch up on the start line
    Line,
    Lined,
    Run,
    Done,
}

/// One character's part in a race. Goals are strip positions in logical px
/// (turned into walk offsets per character, they differ in size).
#[derive(Clone, Copy, Debug)]
pub(super) struct Racer {
    pub(super) leg: Leg,
    pub(super) goal_px: f32,
    pub(super) goal: f32,
    pub(super) speed: f32,
    // seconds until the next stroll while milling about
    pub(super) wait: f32,
    // said hello on reaching the gathering spot
    pub(super) greeted: bool,
    // running: the pace it's aiming for, seconds to the next change of
    // pace, and to the next try at hopping over the one ahead
    pub(super) pace: f32,
    pub(super) next_pace: f32,
    pub(super) hop_cool: f32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Gather,
    Line,
    Run,
    // at the finish: the winner hops in the middle of the crowd
    Cheer,
}

/// The race as a whole, run by the main character.
pub(super) struct Race {
    start_px: f32,
    finish_px: f32,
    // gathering spot: this far from the start line towards the finish
    zone: f32,
    phase: Phase,
    // seconds until the next character catches the racing mood
    next_join: f32,
    // all on the line: seconds to the off
    countdown: f32,
    winner: Option<usize>,
    // who came in, in order
    order: Vec<usize>,
    // the winner hops about this long
    cheer: f32,
    // seconds in this phase, to give up on stuck ones
    age: f32,
}

impl CharacterOverlay {
    pub(super) fn walk_ref(&self, i: usize) -> &WalkState {
        if i == 0 { &self.walk } else { &self.extras[i - 1].walk }
    }

    pub(super) fn walk_of(&mut self, i: usize) -> &mut WalkState {
        if i == 0 { &mut self.walk } else { &mut self.extras[i - 1].walk }
    }

    /// Strip position of a character's centre, logical px.
    pub(super) fn strip_px(&self, i: usize) -> f32 {
        let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
        (r[0] + r[2]) * 0.5 + self.view[0] * self.view_w * 0.5
    }

    /// Very rarely one character wants to race; the mood spreads to the
    /// others one by one, they gather at one end of a big window and wander
    /// about there, and once all have come they bunch up and run to its
    /// other end.
    pub(super) fn run_race(&mut self, dt: f32, w: f32) {
        self.view_w = w;
        let n = self.extras.len() + 1;
        let asked = RACE_NOW.swap(false, std::sync::atomic::Ordering::Relaxed);
        if self.race.is_none() {
            // about once every two hours
            if n < 2 || !(asked || self.walk.rand() < dt / tn().race_every.max(1.0)) {
                return;
            }
            let view_px = self.view[0] * w * 0.5;
            let cols = crate::backend::governor::niri_columns();
            // the widest window, one on screen if there is one
            let seen: Vec<_> = cols.iter().copied().filter(|c| c.0 + c.1 > view_px && c.0 < view_px + w).collect();
            let pool = if seen.is_empty() { &cols } else { &seen };
            let (x, cw) = pool.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1)).unwrap_or((view_px, w));
            let len = cw.max(w * 0.5);
            let mid = x + cw * 0.5;
            let (a, b) = (mid - len * 0.42, mid + len * 0.42);
            let (start_px, finish_px) = if self.walk.rand() < 0.5 { (a, b) } else { (b, a) };
            log::debug!("race: from {start_px:.0} to {finish_px:.0}");
            self.race = Some(Race {
                start_px,
                finish_px,
                zone: (len * tn().race_zone).max(220.0),
                phase: Phase::Gather,
                next_join: 0.0,
                countdown: tn().race_countdown,
                winner: None,
                order: Vec::new(),
                cheer: 0.0,
                age: 0.0,
            });
        }
        let Some(mut race) = self.race.take() else { return };
        race.age += dt;
        let dir = (race.finish_px - race.start_px).signum();
        let (lo, hi) = {
            let far = race.start_px + dir * race.zone;
            (race.start_px.min(far), race.start_px.max(far))
        };
        let grounded = |wk: &WalkState| matches!(wk.act, Act::Idle | Act::Walk);
        let mut over = false;

        match race.phase {
            Phase::Gather => {
                // the mood spreads: the one nearest to someone already in joins next
                race.next_join -= dt;
                if race.next_join <= 0.0 {
                    let joined: Vec<f32> = (0..n).filter(|&i| self.walk_ref(i).race.is_some()).map(|i| self.strip_px(i)).collect();
                    let free: Vec<(usize, f32)> =
                        (0..n).filter(|&i| self.walk_ref(i).race.is_none()).map(|i| (i, self.strip_px(i))).collect();
                    let pick = if joined.is_empty() {
                        let first = self.race_first.take().filter(|&i| i < n);
                        Some(first.unwrap_or((self.walk.rand() * n as f32) as usize % n))
                    } else {
                        let d = |x: f32| joined.iter().map(|p| (p - x).abs()).fold(f32::MAX, f32::min);
                        free.iter().min_by(|a, b| d(a.1).total_cmp(&d(b.1))).map(|f| f.0)
                    };
                    if let Some(i) = pick {
                        let wk = self.walk_of(i);
                        let goal_px = lo + wk.rand() * (hi - lo);
                        let speed = tn().race_gather_speed + wk.rand() * tn().race_gather_rand;
                        wk.race = Some(Racer { leg: Leg::ToStart, goal_px, goal: 0.0, speed, wait: 0.0, greeted: false, pace: 0.0, next_pace: 0.0, hop_cool: 0.0 });
                        wk.circle = false;
                        wk.social = None;
                        // a little hop: "let's race!"
                        if grounded(wk) {
                            wk.start_jump(tn().jump_height * 0.5);
                        }
                    }
                    race.next_join = tn().race_join_min + self.walk.rand() * tn().race_join_rand;
                }
                for i in 0..n {
                    let x = self.strip_px(i);
                    let wk = self.walk_of(i);
                    let Some(mut r) = wk.race else { continue };
                    // stepped into the gathering spot: "I'm here!"
                    if !r.greeted && x >= lo && x <= hi {
                        r.greeted = true;
                        if grounded(wk) {
                            wk.start_jump(tn().jump_height * 0.6);
                        }
                    }
                    // wander about the spot
                    if r.leg == Leg::Mill {
                        r.wait -= dt;
                        if r.wait <= 0.0 {
                            r.goal_px = lo + wk.rand() * (hi - lo);
                            r.speed = tn().race_mill_speed + wk.rand() * tn().race_mill_rand;
                            r.wait = 1.5 + wk.rand() * 2.5;
                        }
                    }
                    wk.race = Some(r);
                }
                let here = (0..n)
                    .filter(|&i| {
                        let wk = self.walk_ref(i);
                        wk.race.is_some_and(|r| r.leg == Leg::Mill) && grounded(wk)
                    })
                    .count();
                // stragglers far along the strip don't hold it up forever
                if here == n || (race.age > tn().race_wait && here >= 2) {
                    for i in 0..n {
                        let start = race.start_px;
                        let wk = self.walk_of(i);
                        match wk.race.as_mut() {
                            Some(r) if r.leg == Leg::Mill => {
                                r.leg = Leg::Line;
                                r.goal_px = start;
                                r.speed = 0.2;
                            }
                            _ => wk.race = None,
                        }
                    }
                    race.phase = Phase::Line;
                    race.age = 0.0;
                }
            }
            Phase::Line => {
                let lined = (0..n).all(|i| {
                    let wk = self.walk_ref(i);
                    wk.race.is_none_or(|r| r.leg == Leg::Lined && grounded(wk))
                });
                if lined {
                    race.countdown -= dt;
                }
                if race.countdown <= 0.0 {
                    // off they go, all at once; starting paces spread evenly
                    // (shuffled) so the bunch pulls apart
                    let mut runners: Vec<usize> = (0..n).filter(|&i| self.walk_ref(i).race.is_some()).collect();
                    for k in (1..runners.len()).rev() {
                        let j = (self.walk.rand() * (k + 1) as f32) as usize % (k + 1);
                        runners.swap(k, j);
                    }
                    let m = runners.len().max(2) as f32 - 1.0;
                    for (k, &i) in runners.iter().enumerate() {
                        let finish = race.finish_px;
                        let wk = self.walk_of(i);
                        let pace = tn().race_pace + k as f32 / m * tn().race_pace_range;
                        let next_pace = 1.0 + wk.rand() * 1.5;
                        if let Some(r) = wk.race.as_mut() {
                            r.leg = Leg::Run;
                            r.goal_px = finish;
                            r.speed = pace;
                            r.pace = pace;
                            r.next_pace = next_pace;
                            r.hop_cool = 0.0;
                        }
                    }
                    race.phase = Phase::Run;
                    race.age = 0.0;
                }
                over = race.age > 60.0;
            }
            Phase::Run => {
                // where everyone is along the track, and how fast
                let width = (self.hit_rect[2] - self.hit_rect[0]).max(1.0);
                let field: Vec<(usize, f32, f32)> = (0..n)
                    .filter_map(|i| {
                        let r = self.walk_ref(i).race?;
                        (r.leg == Leg::Run).then(|| (i, self.strip_px(i) * dir, r.speed))
                    })
                    .collect();
                let settle = race.age > 1.0;
                for &(i, p, _) in &field {
                    let ahead = field.iter().filter(|o| o.1 > p).min_by(|a, b| a.1.total_cmp(&b.1)).copied();
                    let lead = field.iter().filter(|o| o.1 > p).count();
                    let wk = self.walk_of(i);
                    let Some(mut r) = wk.race else { continue };
                    // bursts and lulls
                    r.next_pace -= dt;
                    if r.next_pace <= 0.0 {
                        r.pace = tn().race_pace + wk.rand() * tn().race_pace_range;
                        r.next_pace = tn().race_pace_gap + wk.rand() * 1.7;
                    }
                    r.hop_cool -= dt;
                    let mut want = r.pace;
                    // right on someone's heels: hop over them, or ease off
                    if let Some((_, q, their)) = ahead.filter(|a| settle && a.1 - p < width * 0.45) {
                        if r.hop_cool <= 0.0 {
                            r.hop_cool = 1.5;
                            // only the front runners fight for it by hopping over
                            if lead < tn().race_leaders as usize && wk.rand() < tn().race_hop_chance && grounded(wk) {
                                wk.start_jump(tn().jump_height * 0.55);
                                r.pace = r.pace.max(their + 0.06);
                                r.speed = r.pace;
                            }
                        }
                        if wk.act != Act::Jump && q - p < width * 0.3 {
                            want = want.min(their - 0.02);
                        }
                    }
                    r.speed = (r.speed + (want - r.speed) * (dt * 3.0).min(1.0)).clamp(tn().race_pace * 0.75, tn().race_pace + tn().race_pace_range * 1.6);
                    wk.race = Some(r);
                }
                for i in 0..n {
                    if self.walk_ref(i).race.is_some_and(|r| r.leg == Leg::Done) && !race.order.contains(&i) {
                        race.order.push(i);
                        if race.winner.is_none() {
                            race.winner = Some(i);
                            let wk = self.walk_of(i);
                            if grounded(wk) {
                                wk.start_jump(tn().jump_height * 1.2);
                            }
                        }
                    }
                }
                let all_done = (0..n).all(|i| self.walk_ref(i).race.is_none_or(|r| r.leg == Leg::Done));
                if all_done && race.winner.is_some() {
                    // gather round: the winner in the middle, the rest on both
                    // sides in finishing order, half overlapping like a crowd
                    let spacing = width * 0.5;
                    let m = race.order.len() as f32;
                    let centre = race.finish_px - dir * spacing * (m * 0.5).floor();
                    for (k, &i) in race.order.clone().iter().enumerate() {
                        let wk = self.walk_of(i);
                        let side = if k % 2 == 1 { 1.0 } else { -1.0 };
                        let slot = k.div_ceil(2) as f32;
                        let jitter = (wk.rand() - 0.5) * spacing * 0.3;
                        let goal_px = if k == 0 { centre } else { centre + side * slot * spacing + jitter };
                        if let Some(r) = wk.race.as_mut() {
                            r.leg = Leg::Line;
                            r.goal_px = goal_px;
                            r.speed = 0.45;
                        }
                    }
                    race.phase = Phase::Cheer;
                    race.cheer = tn().race_cheer;
                } else {
                    over = race.age > 60.0;
                }
            }
            Phase::Cheer => {
                race.cheer -= dt;
                if let Some(i) = race.winner {
                    let wk = self.walk_of(i);
                    if race.cheer > 0.0 && grounded(wk) && wk.race.is_some_and(|r| r.leg == Leg::Lined) {
                        wk.start_jump(tn().jump_height * 0.9);
                    }
                }
                over = race.cheer <= 0.0;
            }
        }
        if over || race.age > 300.0 {
            log::debug!("race: over, winner {:?}", race.winner);
            for i in 0..n {
                let wk = self.walk_of(i);
                let raced = wk.race.take().is_some();
                if raced {
                    wk.crowd_rest = tn().crowd_rest_base + wk.rand() * tn().crowd_rest_rand;
                }
                if raced && Some(i) != race.winner && grounded(wk) {
                    wk.start_jump(tn().jump_height * 0.4);
                }
            }
            return;
        }
        self.race = Some(race);
    }
}

impl WalkState {
    /// A racer on the ground: walk to the goal the race gives, stand about
    /// once there. None when not racing (or busy jumping, held, ...).
    pub(super) fn race_step(&mut self, dt: f32) -> Option<(f32, bool, f32)> {
        if let Some(r) = self.race.as_mut().filter(|_| matches!(self.act, Act::Idle | Act::Walk | Act::Chase)) {
            // carried off the start line: back to it
            if r.leg == Leg::Lined && (r.goal - self.x).abs() > 0.02 {
                r.leg = Leg::Line;
            }
            let walking = matches!(r.leg, Leg::ToStart | Leg::Mill | Leg::Line | Leg::Run);
            let d = r.goal - self.x;
            let s = r.speed * dt;
            if walking && d.abs() > s {
                self.x += d.signum() * s;
                self.left = d < 0.0;
                self.target = r.goal;
                self.act = Act::Walk;
                return Some((self.x, true, 0.0));
            }
            if walking {
                self.x = r.goal;
                r.leg = match r.leg {
                    Leg::ToStart => Leg::Mill,
                    Leg::Line => Leg::Lined,
                    Leg::Run => Leg::Done,
                    l => l,
                };
            }
            self.act = Act::Idle;
            self.timer = 1.0;
            self.target = self.x;
            return Some((self.x, false, 0.0));
        }
        None
    }

    /// Hopping over someone mid-race: keep going.
    pub(super) fn race_hop(&mut self, dt: f32) {
        if let Some(r) = self.race.as_mut().filter(|r| r.leg == Leg::Run) {
            let d = r.goal - self.x;
            let s = r.speed * dt;
            if d.abs() <= s {
                self.x = r.goal;
                r.leg = Leg::Done;
            } else {
                self.x += d.signum() * s;
                self.left = d < 0.0;
            }
        }
    }
}
