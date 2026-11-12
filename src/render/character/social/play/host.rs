//! The park attendant: calls game time with the megaphone, gathers a few,
//! stands at the side and keeps an eye on the game, calls time when it drags
//! on, hands the winner a prize, asks those standing about to come and play,
//! and helps up whoever came down hard.

use super::*;

/// Helping someone up: (attendant, who, since).
static HELP: Mutex<Option<(usize, usize, Instant)>> = Mutex::new(None);

impl CharacterOverlay {
    /// The attendant at work in the park and free, if there is one.
    pub(super) fn park_host(&self, n: usize) -> Option<usize> {
        (0..n).find(|&i| {
            self.job_now(i).as_deref() == Some("attendant")
                && self.place_here(i).is_some_and(|p| p.place == "park")
                && self.free_for(i, Want::Work)
                && !self.hungry(i)
        })
    }

    /// `h` hosts the game chat `c`: the megaphone up, two hops, to the middle.
    pub(super) fn host_call(&mut self, c: &mut Chat, h: usize, middle: f32) {
        self.enlist(h, c.id);
        c.host = Some(h);
        let wk = self.walk_of(h);
        wk.gesture = true;
        wk.start_bob(tn().jump_height * 0.35);
        wk.notify_jump = Some(0.5);
        self.go(h, middle, 0.16);
        log::debug!("chat {}: the attendant {h} calls game time", c.id);
    }

    /// Every frame of a hosted game: beside it, watching; time called when
    /// it drags on; the winner's prize; off again once they're talking.
    pub(in crate::render::character::social) fn host_tick(&mut self, c: &mut Chat) {
        let Some(h) = c.host else { return };
        let wk = self.walk_ref(h);
        if !wk.social.is_some_and(|s| s.chat == c.id) || matches!(wk.act, Act::Held | Act::Fall) {
            c.host = None;
            self.leave(h, c.id);
            return;
        }
        if c.members.is_empty() {
            return;
        }
        let talk = matches!(c.stage, Stage::Talk | Stage::Part | Stage::Stroll);
        if talk && c.games > 0 {
            // the game's done: back to looking after the park
            log::debug!("chat {}: the attendant {h} goes back to the park", c.id);
            self.work(h, "attendant", 2.0);
            c.host = None;
            self.leave(h, c.id);
            return;
        }
        let xs: Vec<f32> = c.members.iter().map(|&i| self.strip_px(i)).collect();
        let (lo, hi) = (xs.iter().copied().fold(f32::MAX, f32::min), xs.iter().copied().fold(f32::MIN, f32::max));
        let size = self.width_of(h);
        let x = self.strip_px(h);
        if !matches!(c.stage, Stage::Notice | Stage::Approach) {
            // at the side, out of the way, looking on
            let goal = if (x - lo).abs() < (x - hi).abs() { lo - size * 1.6 } else { hi + size * 1.6 };
            let goal = goal.clamp(size, (self.strip_width_of(h) - size).max(size));
            if (x - goal).abs() > size * 1.2 && self.arrived(h) {
                self.go(h, goal, 0.14);
            } else if self.arrived(h) {
                let left = (lo + hi) * 0.5 < x;
                if let Some(r) = self.walk_of(h).social.as_mut() {
                    r.face_left = left;
                }
            }
        }
        // dragging on: time's up
        let game = !matches!(c.stage, Stage::Notice | Stage::Approach | Stage::Talk | Stage::Stroll | Stage::Part | Stage::Cheer | Stage::Play);
        if game && !c.host_called && c.stage_t > super::super::life::stage_limit(c.stage) * 0.6 {
            c.host_called = true;
            log::debug!("chat {}: the attendant calls time", c.id);
            let wk = self.walk_of(h);
            wk.gesture = true;
            wk.start_bob(tn().jump_height * 0.35);
            self.end_game(c);
            return;
        }
        // the winner gets a prize from the park
        if c.stage == Stage::Cheer && !c.host_paid {
            c.host_paid = true;
            if let Some(w) = c.winner.filter(|w| c.members.contains(w)) {
                let name = self.character_name(w);
                // from the piggy bank while there's anything in it
                let prize = tn().host_prize as u32;
                if !park::pay("Park", prize, &[(name.as_str(), 1.0)]) {
                    park::earn(&name, prize);
                }
                // and the attendant is paid for calling it
                let hname = self.character_name(h);
                park::pay_upto("Park", tn().host_fee as u32, &hname);
                log::debug!("chat {}: the attendant gives {name} {} coins", c.id, tn().host_prize);
                let wk = self.walk_of(h);
                wk.gesture = true;
                wk.start_bob(tn().jump_height * 0.3);
            }
        }
    }

    /// `i` has been standing about in the park: the attendant waves it over.
    pub(super) fn host_invite(&mut self, i: usize, n: usize) {
        let Some(h) = self.park_host(n).filter(|&h| h != i) else { return };
        let left = self.strip_px(i) < self.strip_px(h);
        let wk = self.walk_of(h);
        wk.left = left;
        wk.gesture = true;
        wk.start_bob(tn().jump_height * 0.25);
        log::debug!("park: the attendant {h} asks {i} to come and play");
    }

    /// Someone in the park came down hard: the attendant goes over, and once
    /// there they both laugh it off.
    pub(super) fn host_help(&mut self, n: usize) {
        let mut help = HELP.lock().unwrap();
        if let Some((h, who, since)) = *help {
            let near = (self.strip_px(h) - self.strip_px(who)).abs() < self.width_of(h) * 1.3;
            if near || since.elapsed().as_secs_f32() > 12.0 || self.walk_ref(h).social.is_some() {
                if near {
                    let secs = tn().laugh_time;
                    self.walk_of(h).laugh(secs);
                    self.walk_of(who).laugh(secs);
                    self.work(h, "attendant", 0.5);
                    log::debug!("park: the attendant {h} helps {who} up");
                }
                *help = None;
            }
            return;
        }
        let Some(h) = self.park_host(n) else { return };
        let Some(who) = (0..n).find(|&i| i != h && self.walk_ref(i).act == Act::Planted && self.walk_ref(i).ws == self.walk_ref(h).ws && self.place_here(i).is_some_and(|p| p.place == "park")) else { return };
        let (from, to) = (self.strip_px(h), self.strip_px(who));
        let side = if from < to { -1.0 } else { 1.0 };
        let goal = to + side * self.width_of(h) * 0.8;
        let surface = self.actor_ref(h).surface[0].max(1.0);
        let wk = self.walk_of(h);
        wk.target = wk.x + (goal - from) * 2.0 / surface;
        wk.act = Act::Walk;
        wk.timer = 6.0;
        *help = Some((h, who, Instant::now()));
        log::debug!("park: the attendant {h} goes to help {who}");
    }
}
