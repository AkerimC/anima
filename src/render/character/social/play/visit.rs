//! Mode 3, "live on the desktop, visit the windows": the characters live on
//! the user's workspaces and come over to Anima Park for game time, returning
//! to their desktop workspace once finished.

use super::*;

pub(super) const GAME: u8 = 8;
pub(super) const DONE: u8 = 9;

/// Mode 3 is on (and Anima Park is open).
pub(super) fn visiting_mode() -> bool {
    (2.5..3.5).contains(&tn().park_mode) && !governor::places().is_empty()
}

impl CharacterOverlay {
    /// On the user's desktop and free to go over for a game.
    fn desk_free(&self, i: usize, windows: &[u64]) -> bool {
        let wk = self.walk_ref(i);
        wk.at_leisure() && wk.trip.is_none() && wk.rest.is_none() && !wk.visiting && !wk.backstage && wk.visit_for == 0 && !windows.contains(&wk.ws)
    }

    /// Sends `i` over to Anima Park for `reason`.
    pub(super) fn send_visit(&mut self, i: usize, place: &str, reason: u8) {
        let Some(p) = governor::place_rect(place) else { return };
        let from = self.walk_ref(i).ws;
        let wk = self.walk_of(i);
        wk.visit_for = reason;
        wk.visit_t = 0.0;
        if !governor::places().iter().any(|q| q.ws == from) {
            wk.visit_from = from;
        }
        if from == p.ws {
            self.walk_into(i, &p);
        } else {
            self.send_to(i, p.ws);
        }
        log::debug!("visit: {i} off to the {place} ({reason})");
    }

    /// Every frame in mode 3: keeps visits going and sends finished ones home.
    pub(super) fn visit_tick(&mut self, chats: &mut Vec<Chat>, dt: f32, n: usize) {
        if !visiting_mode() {
            for i in 0..n {
                if self.walk_ref(i).visit_for != 0 {
                    self.walk_of(i).visit_for = 0;
                }
            }
            return;
        }
        let windows: Vec<u64> = governor::places().into_iter().map(|p| p.ws).collect();
        for i in 0..n {
            let wk = self.walk_of(i);
            if wk.visit_for != 0 {
                wk.visit_t += dt;
            }
        }
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut next = NEXT.lock().unwrap();
            if next.is_some_and(|t| t.elapsed().as_secs_f32() < 1.0) {
                return;
            }
            *next = Some(Instant::now());
        }
        let in_chat = |chats: &Vec<Chat>, i: usize| chats.iter().any(|c| c.members.contains(&i) || c.watchers.contains(&i));
        for i in 0..n {
            let (reason, t, ws, here_t) = {
                let wk = self.walk_ref(i);
                (wk.visit_for, wk.visit_t, wk.ws, wk.here_t)
            };
            let on_windows = windows.contains(&ws);
            let free = self.walk_ref(i).at_leisure() && self.walk_ref(i).trip.is_none() && !in_chat(chats, i);
            if reason == 0 {
                if on_windows && free && here_t > 5.0 {
                    let wk = self.walk_of(i);
                    wk.visit_for = DONE;
                    wk.visit_t = 20.0;
                }
                continue;
            }
            if !on_windows && reason != DONE {
                if t > 60.0 && self.walk_ref(i).trip.is_none() {
                    self.walk_of(i).visit_for = 0;
                }
                continue;
            }
            let inside = self.place_here(i).is_some_and(|p| p.place == "park");
            if reason != DONE && free && !inside && !self.walk_ref(i).social.is_some_and(|s| s.moving) {
                if let Some(p) = governor::place_rect("park") {
                    if self.walk_ref(i).act == Act::Idle {
                        self.walk_into(i, &p);
                    }
                }
            }
            if reason == GAME && t > 45.0 && free {
                let wk = self.walk_of(i);
                wk.visit_for = DONE;
                wk.visit_t = 0.0;
            }
            if reason == DONE && free && t > 25.0 {
                let from = self.walk_ref(i).visit_from;
                if let Some(home) = self.desktop_home(from).filter(|&home| home != ws) {
                    self.send_to(i, home);
                }
                self.walk_of(i).visit_for = 0;
            }
        }
    }

    /// Game time in mode 3: a few from the desktop go over to the park for it.
    pub(super) fn game_volunteers(&mut self, n: usize) -> usize {
        let windows: Vec<u64> = governor::places().into_iter().map(|p| p.ws).collect();
        let free: Vec<usize> = (0..n).filter(|&i| self.desk_free(i, &windows) && self.walk_ref(i).energy >= tn().energy_game_need).collect();
        let on_desk = (0..n).filter(|&i| !windows.contains(&self.walk_ref(i).ws) && self.walk_ref(i).visit_for == 0).count();
        let k = free.len().min(4).min(on_desk.saturating_sub(3));
        for &i in free.iter().take(k) {
            self.send_visit(i, "park", GAME);
        }
        k
    }
}
