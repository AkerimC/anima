//! Out on the desktop: noticing each other, saying hello, joining a chat,
//! calling a friend over, going off to find company.

use super::super::CharacterOverlay;
use super::super::walk::Act;
use super::*;
use crate::render::tuning::get as tn;

impl CharacterOverlay {
    /// Two standing close, on screen and free: maybe they notice each other.
    /// `room`: not too many chats going on (standing inside each other is
    /// worth one anyway).
    pub(in crate::render::character) fn find_pair(&mut self, n: usize, room: bool) -> Option<Chat> {
        let free: Vec<(usize, f32, f32, f32)> = (0..n)
            .filter(|&i| {
                self.free_for(i, Want::Pair)
            })
            .map(|i| (i, self.strip_px(i), self.width_of(i), chat_cool(self.walk_ref(i))))
            .collect();
        let mut overlap: Option<(f32, usize, usize)> = None;
        let mut near: Option<(f32, usize, usize)> = None;
        let mut crowded: Option<(f32, usize, usize)> = None;
        for (k, p) in free.iter().enumerate() {
            for q in &free[k + 1..] {
                if self.walk_ref(p.0).ws != self.walk_ref(q.0).ws {
                    continue;
                }
                let d = (p.1 - q.1).abs();
                let size = (p.2 + q.2) * 0.5;
                // old friends notice each other from further off, rivals less
                let (friend, rival) = self.bond(p.0, q.0);
                let pull = 1.0 + friend * tn().bond_pull;
                let rank = d / pull * (1.0 + rival * 0.5);
                let slot = if d < size * tn().overlap {
                    if p.3 <= tn().overlap_cool && q.3 <= tn().overlap_cool { &mut overlap } else { &mut crowded }
                } else if d < size * tn().near_range * pull && p.3 <= 0.0 && q.3 <= 0.0 {
                    &mut near
                } else {
                    continue;
                };
                // ties go either way, not to whoever is first in the team
                let rank = rank * (1.0 + self.walk.rand() * 0.02);
                if slot.is_none_or(|s| rank < s.0) {
                    *slot = Some((rank, p.0, q.0));
                }
            }
        }
        let roll = self.walk.rand();
        let pair = match (overlap, near) {
            (Some(o), _) if roll < tn().overlap_chance => o,
            (_, Some(o)) if room && roll < tn().near_chance * self.fewness(o.1) => o,
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
        // now they've met
        if self.bond(a, b).0 < tn().first_meet {
            self.add_bond(a, b, tn().first_meet, 0.0);
        }
        // friends are pleased to see each other
        if self.bond(a, b).0 >= tn().bond_friend {
            for i in [a, b] {
                let wk = self.walk_of(i);
                wk.notify_jump = Some(0.1 + wk.rand() * 0.3);
            }
        }
        log::debug!("chat {id}: {a} and {b}{}", if pair.0 < self.width_of(a) * tn().overlap { " (were inside each other)" } else { "" });
        Some(Chat::new(id, vec![a, b], Stage::Notice, timer))
    }
    /// Someone new on the workspace: whoever knows it least goes to say hello.
    pub(in crate::render::character) fn greet_newcomer(&mut self, i: usize, n: usize) {
        if self.social_age < tn().warm_alone || self.moods.fretting {
            return;
        }
        let (ws, x) = (self.walk_ref(i).ws, self.strip_px(i));
        let view = self.view_w.max(1.0);
        let host = (0..n)
            .filter(|&j| j != i && self.walk_ref(j).ws == ws && self.walk_ref(j).at_leisure())
            .map(|j| (j, self.bond(i, j).0 * 2.0 + (self.strip_px(j) - x).abs() / view))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let Some((host, _)) = host else { return };
        self.chat_id = self.chat_id.wrapping_add(1);
        let id = self.chat_id;
        self.enlist(host, id);
        self.enlist(i, id);
        self.face(host, i);
        log::debug!("chat {id}: {host} comes to meet newcomer {i}");
        let timer = 1.2 + self.walk.rand() * 0.8;
        self.new_chats.push(Chat::new(id, vec![host, i], Stage::Notice, timer));
    }
    /// Standing inside someone with nothing to say: one shuffles over.
    pub(in crate::render::character) fn step_aside(&mut self, i: usize, other: usize, d: f32) {
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
        if !(0.0..=self.strip_width_of(i)).contains(&(x + dir * dist)) {
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
    pub(in crate::render::character) fn try_join(&mut self, c: &mut Chat, n: usize) {
        let spots: Vec<f32> = c.members.iter().map(|&i| self.strip_px(i)).collect();
        let size = self.width_of(c.members[0]);
        let free: Vec<(usize, f32, f32)> = (0..n)
            .filter(|&i| {
                self.walk_ref(i).ws == self.walk_ref(c.members[0]).ws && self.free_for(i, Want::Join)
            })
            .map(|i| {
                let x = self.strip_px(i);
                let friend = c.members.iter().map(|&m| self.bond(i, m).0).fold(0.0, f32::max);
                (i, spots.iter().map(|p| (p - x).abs()).fold(f32::MAX, f32::min), friend)
            })
            .filter(|f| f.1 < size * join_range(c.members.len()) * (1.0 + f.2 * tn().bond_pull))
            .collect();
        let Some(&(i, _, friend)) = free.iter().min_by(|a, b| (a.1 / (1.0 + a.2 * tn().bond_pull)).total_cmp(&(b.1 / (1.0 + b.2 * tn().bond_pull)))) else { return };
        let moving = c.stage == Stage::Stroll || self.walk_ref(i).act == Act::Walk;
        let chance = join_chance(c.members.len()) * if moving { tn().walk_join } else { 1.0 } * (1.0 + friend);
        if self.walk.rand() >= chance {
            return;
        }
        self.spend(i, tn().energy_join_cost);
        self.join(c, i);
    }
    /// Someone standing inside a member of a chat: a chat right there is
    /// hard to miss, so they join; if they've only just had one, they
    /// shuffle out of the way instead. True if it dealt with someone.
    pub(in crate::render::character) fn overlap_join(&mut self, c: &mut Chat, n: usize, full: bool) -> bool {
        let mut hit: Option<(f32, usize, usize)> = None;
        for i in 0..n {
            if self.walk_ref(i).ws != self.walk_ref(c.members[0]).ws || !self.free_for(i, Want::Overlap) {
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
            self.join(c, i);
        } else if !full && chat_cool(self.walk_ref(i)) <= tn().overlap_cool {
            if self.walk.rand() < tn().overlap_chance {
                self.join(c, i);
            }
        } else if self.walk.rand() < tn().step_chance {
            self.step_aside(i, m, d);
        }
        true
    }
    pub(in crate::render::character) fn join(&mut self, c: &mut Chat, i: usize) {
        c.quiet = 0.0;
        self.enlist(i, c.id);
        if !matches!(c.stage, Stage::Notice | Stage::Approach | Stage::Talk | Stage::Stroll | Stage::Part | Stage::RaceCall | Stage::RaceWait) {
            if let Some(r) = self.walk_of(i).social.as_mut() {
                r.no_pat = true;
            }
        }
        // someone here it knows: they introduce it to the rest
        let friend = c.members.iter().copied().find(|&m| self.bond(i, m).0 >= tn().meet_friend);
        if let Some(f) = friend {
            for m in c.members.clone() {
                if m != f && self.bond(i, m).0 < tn().intro_bond {
                    log::debug!("chat {}: {f} introduces {i} to {m}", c.id);
                    self.add_bond(i, m, tn().intro_bond, 0.0);
                }
            }
        }
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
                // nobody waits for them and nobody shuffles along: the
                // newcomer walks round to the end of the row, looking in
                self.catch_up(c, i);
                let mean = c.members.iter().filter(|&&m| m != i).map(|&m| self.strip_px(m)).sum::<f32>() / (c.members.len() - 1).max(1) as f32;
                let left_of_row = self.strip_px(i) < mean;
                let pace = 0.11 + self.walk.rand() * 0.05;
                if let Some(r) = self.walk_of(i).social.as_mut() {
                    r.face_left = !left_of_row;
                    r.speed = pace;
                }
                if c.stage == Stage::Contest {
                    c.queue.push(i);
                } else if c.stage == Stage::Talk {
                    c.timer = self.talk_length() + Self::crowd_talk(c);
                }
            }
        }
    }
    /// Someone in the chat spots a friend wandering past and waves it over.
    pub(in crate::render::character) fn call_friend_over(&mut self, c: &mut Chat, n: usize, every: f32) {
        if c.members.len() >= self.group_cap() || self.walk.rand() >= every * tn().intro_call / 60.0 {
            return;
        }
        let ws = self.walk_ref(c.members[0]).ws;
        let range = self.view_w * 0.6;
        let mut best: Option<(usize, usize, f32)> = None;
        for &m in &c.members {
            let x = self.strip_px(m);
            for f in 0..n {
                if c.members.contains(&f) || self.walk_ref(f).ws != ws || !self.free_for(f, Want::Called) || (self.strip_px(f) - x).abs() > range {
                    continue;
                }
                let b = self.bond(m, f).0;
                if b >= tn().meet_friend && best.is_none_or(|x| b > x.2) {
                    best = Some((m, f, b));
                }
            }
        }
        if let Some((m, f, _)) = best {
            log::debug!("chat {}: {m} calls {f} over", c.id);
            self.walk_of(m).start_bob(tn().jump_height * 0.25);
            self.join(c, f);
        }
    }
    /// Few on this workspace: they go looking for company instead of
    /// waiting to bump into someone. Called a few times a second.
    pub(in crate::render::character) fn seek_company(&mut self, n: usize, every: f32) {
        for i in 0..n {
            if !self.free_for(i, Want::Seek) {
                continue;
            }
            let ws = self.walk_ref(i).ws;
            let here: Vec<usize> = (0..n).filter(|&j| j != i && self.walk_ref(j).ws == ws && self.walk_ref(j).act != Act::Travel).collect();
            if here.is_empty() {
                continue;
            }
            // the fewer around, the keener; a friend in sight is worth going to anyway
            let few = (tn().seek_few / (here.len() + 1) as f32).clamp(0.0, 3.0);
            let x0 = self.strip_px(i);
            let friend_near = here.iter().any(|&j| self.bond(i, j).0 >= tn().meet_friend && (self.strip_px(j) - x0).abs() < self.view_w * 0.7);
            let keen = if friend_near { few.max(1.5) } else { few };
            if keen < 1.0 || self.walk_of(i).rand() >= every * keen / tn().seek_every.max(1.0) {
                continue;
            }
            let x = self.strip_px(i);
            // the best friend there, else whoever is nearest
            let pick = here
                .iter()
                .copied()
                .max_by(|a, b| {
                    let score = |j: usize| self.bond(i, j).0 * 600.0 - (self.strip_px(j) - x).abs();
                    score(*a).total_cmp(&score(*b))
                })
                .unwrap_or(here[0]);
            let to = self.strip_px(pick);
            let side = if x < to { -1.0 } else { 1.0 };
            let goal = to + side * self.width_of(pick) * 0.9;
            let view = self.view_w.max(1.0);
            let wk = self.walk_of(i);
            wk.target = wk.x + (goal - x) * 2.0 / view;
            wk.act = Act::Walk;
            log::debug!("{i} goes to find {pick}");
        }
    }
    /// Friends walking past each other stop for a happy little wave.
    pub(in crate::render::character) fn wave_at_friends(&mut self, n: usize) {
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = (self.walk_ref(i), self.walk_ref(j));
                let free = |w: &WalkState| w.act == Act::Walk && w.social.is_none() && w.race.is_none() && w.crowd.is_none() && w.greet_cool <= 0.0 && !w.fret;
                if !free(a) || !free(b) || a.ws != b.ws {
                    continue;
                }
                let (xi, xj) = (self.strip_px(i), self.strip_px(j));
                if (xi - xj).abs() > self.width_of(i) * 1.3 || self.bond(i, j).0 < tn().bond_friend {
                    continue;
                }
                log::debug!("{i} and {j} wave at each other");
                for (k, other_x, me_x) in [(i, xj, xi), (j, xi, xj)] {
                    let wk = self.walk_of(k);
                    wk.left = other_x < me_x;
                    wk.act = Act::Idle;
                    wk.timer = 1.0 + wk.rand() * 0.6;
                    wk.target = wk.x;
                    wk.greet_cool = tn().greet_cool;
                    wk.start_jump(tn().jump_height * 0.35);
                }
            }
        }
    }
}
