//! Petting in company: whoever is near, or chatting with the one being
//! petted, shuffles up beside it wanting a turn, spread out so each can be
//! reached, then goes back to what it was doing.

use crate::render::tuning::get as tn;
use super::CharacterOverlay;
use super::walk::{Act, WalkState};

#[derive(Clone, Copy, Debug)]
pub(super) struct PatCrowd {
    goal: f32,
    home: f32,
    // seconds it keeps waiting after the petting stops
    linger: f32,
    // walking back to `home`
    back: bool,
    face_left: bool,
}

impl PatCrowd {
    /// Going to have a look at something, not waiting for petting.
    pub(super) fn curious(goal: f32, home: f32, linger: f32, face_left: bool) -> Self {
        Self { goal, home, linger, back: false, face_left }
    }
}

impl WalkState {
    /// Waiting for a turn: walk up, stand facing the one being petted.
    pub(super) fn crowd_step(&mut self, dt: f32, min: f32, max: f32) -> Option<(f32, bool, f32)> {
        let c = self.crowd?;
        if !matches!(self.act, Act::Idle | Act::Walk) {
            return None;
        }
        if self.race.is_some() || self.circle {
            self.crowd = None;
            return None;
        }
        let (goal, speed) = if c.back { (c.home, tn().walk_speed) } else { (c.goal, tn().pat_crowd_speed) };
        let goal = goal.clamp(min, max);
        let d = goal - self.x;
        if d.abs() > speed * dt {
            self.x += d.signum() * speed * dt;
            self.left = d < 0.0;
            self.act = Act::Walk;
            self.target = goal;
            return Some((self.x, true, 0.0));
        }
        self.x = goal;
        self.act = Act::Idle;
        self.timer = 1.0;
        self.target = goal;
        if c.back {
            self.crowd = None;
            return Some((self.x, false, 0.0));
        }
        self.left = c.face_left;
        if self.rand() < dt * tn().pat_hop {
            self.start_jump(tn().jump_height * 0.5);
        }
        Some((self.x, false, 0.0))
    }

    fn crowd_tick(&mut self, dt: f32, petting: bool) {
        let Some(mut c) = self.crowd else { return };
        if matches!(self.act, Act::Held | Act::Fall | Act::Planted) || self.social.is_some_and(|s| s.no_touch || s.no_pat) {
            self.crowd = None;
            return;
        }
        if c.back {
            return;
        }
        if petting {
            c.linger = tn().pat_linger;
        } else {
            c.linger -= dt;
            if c.linger <= 0.0 {
                // only a chat has a place to go back to
                if self.social.is_none() {
                    self.crowd = None;
                    return;
                }
                c.back = true;
            }
        }
        self.crowd = Some(c);
    }
}

/// Which side to wait on and how many places out: always the side it's
/// already on, so nobody crosses over. None when that side is full.
fn slot(from: f32, anchor: f32, left: usize, right: usize, max: usize) -> Option<(bool, usize)> {
    let on_right = if (from - anchor).abs() < 2.0 { right < left } else { from > anchor };
    let taken = if on_right { right } else { left };
    (taken < max).then_some((on_right, taken + 1))
}

impl CharacterOverlay {
    fn box_of(&self, i: usize) -> [f32; 4] {
        if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect }
    }

    /// `p` was just petted: who wants a turn too, and where each one stands.
    pub(super) fn gather_for_pat(&mut self, p: usize) {
        let n = self.extras.len() + 1;
        let middle = |r: [f32; 4]| (r[0] + r[2]) * 0.5;
        let rect = self.box_of(p);
        let width = rect[2] - rect[0];
        let screen = self.surface[0].max(1.0);
        if width <= 0.0 {
            return;
        }
        let anchor = middle(rect);
        let spacing = width * tn().pat_fan;
        let chat = self.walk_ref(p).social.map(|s| s.chat);
        let mut joiners: Vec<(f32, usize)> = Vec::new();
        for i in (0..n).filter(|&i| i != p) {
            let from = middle(self.box_of(i));
            let dist = (from - anchor).abs();
            let roll = self.walk_of(p).rand();
            let wk = self.walk_ref(i);
            let free = wk.ws == self.walk_ref(p).ws
                && wk.crowd.is_none()
                && matches!(wk.act, Act::Idle | Act::Walk | Act::Jump)
                && wk.race.is_none()
                && !wk.circle
                && !wk.social.is_some_and(|s| s.no_touch || s.no_pat);
            let mate = chat.is_some() && wk.social.map(|s| s.chat) == chat && roll < tn().pat_mate_chance;
            let on_top = dist < spacing;
            let nearby = wk.social.is_none() && dist < tn().pat_gather * screen * 0.5 && roll < tn().pat_gather_chance;
            if free && (mate || on_top || nearby) {
                joiners.push((dist, i));
            }
        }
        joiners.sort_by(|a, b| a.0.total_cmp(&b.0));
        // those already waiting keep their places
        let waiting = |this: &Self, on_right: bool| {
            (0..n).filter(|&i| this.walk_ref(i).crowd.is_some_and(|c| !c.back && c.face_left == on_right)).count()
        };
        let (mut left, mut right) = (waiting(self, false), waiting(self, true));
        for (_, i) in joiners {
            let from = middle(self.box_of(i));
            let Some((on_right, place)) = slot(from, anchor, left, right, tn().pat_side_max as usize) else { continue };
            let k = if on_right { place as f32 } else { -(place as f32) };
            let walk = (anchor + k * spacing - from) * 2.0 / screen;
            // nobody treks across the screen for it
            if walk.abs() > tn().pat_walk_max {
                continue;
            }
            if on_right {
                right = place;
            } else {
                left = place;
            }
            let wk = self.walk_of(i);
            let goal = wk.x + walk;
            wk.crowd = Some(PatCrowd { goal, home: wk.x, linger: tn().pat_linger, back: false, face_left: on_right });
        }
    }

    pub(super) fn run_pat_crowd(&mut self, dt: f32, n: usize) {
        let petting = (0..n).any(|i| self.walk_ref(i).act == Act::Pat);
        for i in 0..n {
            self.walk_of(i).crowd_tick(dt, petting);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    fn waiter(x: f32, goal: f32) -> WalkState {
        let mut wk = WalkState { act: Act::Idle, timer: 100.0, seed: 5, x, target: x, ..Default::default() };
        wk.crowd = Some(PatCrowd { goal, home: x, linger: 1.0, back: false, face_left: true });
        wk
    }

    #[test]
    fn walks_up_and_faces_the_one_being_petted() {
        let mut wk = waiter(0.0, 0.3);
        for _ in 0..60 * 5 {
            if wk.act == Act::Jump {
                wk.act = Act::Idle;
            }
            wk.crowd_step(DT, -1.0, 1.0);
        }
        assert!((wk.x - 0.3).abs() < 1e-3, "at {}", wk.x);
        assert!(wk.left);
    }

    #[test]
    fn stays_while_petting_goes_on_then_leaves() {
        let mut wk = waiter(0.0, 0.0);
        for _ in 0..60 * 10 {
            wk.crowd_tick(DT, true);
        }
        assert!(wk.crowd.is_some(), "left during the petting");
        for _ in 0..60 * 10 {
            wk.crowd_tick(DT, false);
        }
        assert!(wk.crowd.is_none());
    }

    #[test]
    fn goes_back_to_where_it_stood() {
        let mut wk = waiter(0.1, 0.5);
        for _ in 0..60 * 5 {
            if wk.act == Act::Jump {
                wk.act = Act::Idle;
            }
            wk.crowd_step(DT, -1.0, 1.0);
        }
        wk.crowd.as_mut().unwrap().back = true;
        for _ in 0..60 * 10 {
            if wk.crowd.is_none() {
                break;
            }
            wk.crowd_step(DT, -1.0, 1.0);
        }
        assert!(wk.crowd.is_none());
        assert!((wk.x - 0.1).abs() < 1e-3, "at {}", wk.x);
    }

    #[test]
    fn keeps_to_its_own_side() {
        assert_eq!(slot(100.0, 500.0, 2, 0, 3), Some((false, 3)), "stays left though that side is busier");
        assert_eq!(slot(900.0, 500.0, 0, 1, 3), Some((true, 2)));
        assert_eq!(slot(100.0, 500.0, 3, 0, 3), None, "full: stays where it is");
        assert_eq!(slot(500.0, 500.0, 1, 0, 3), Some((true, 1)), "dead centre takes the emptier side");
    }

    #[test]
    fn being_picked_up_ends_the_wait() {
        let mut wk = waiter(0.0, 0.3);
        wk.act = Act::Held;
        wk.crowd_tick(DT, true);
        assert!(wk.crowd.is_none());
    }
}
