//! Mode 3, "live on the desktop, visit the windows": the characters live on
//! the user's workspaces and go over to the park, cafe or farm only for a
//! reason, coming back once it's done:
//! - mealtimes: most go to the cafe to eat (a few stay behind);
//! - hungry in between: to the cafe;
//! - the chef and waiters: to work while there are diners;
//! - eggs, milk and crops piling up on the farm: the farmer goes to collect;
//! - now and then the fisher goes to fish off the pier;
//! - now and then the park attendant looks in on the park;
//! - game time in the park: a few volunteers.
//! Each visit has a place it heads for and a clear end, and a time limit, so
//! nobody hangs about or goes back and forth for nothing.

use super::*;

pub(super) const MEAL: u8 = 1;
pub(super) const HUNGRY: u8 = 2;
pub(super) const FARM: u8 = 3;
pub(super) const FISH: u8 = 4;
pub(super) const FISHING: u8 = 5;
pub(super) const WORK: u8 = 6;
pub(super) const ATTEND: u8 = 7;
pub(super) const GAME: u8 = 8;
pub(super) const DONE: u8 = 9;
// came to eat and found nothing: home, and a nap
pub(super) const NO_FOOD: u8 = 10;
// work first: at its workplace for as long as that's on
pub(super) const ON_SHIFT: u8 = 11;

/// Mode 3 is on (and there's somewhere to visit).
pub(super) fn visiting_mode() -> bool {
    (2.5..3.5).contains(&tn().park_mode) && !governor::places().is_empty()
}

/// Which mealtime it is now, as a number of its own (day and meal), if any.
pub(super) fn meal_slot() -> Option<u32> {
    if !hunger::mealtime() {
        return None;
    }
    let day = (park::now() / 86400.0) as u32;
    let m = super::super::super::weather::minutes_now();
    let k = if m < 11 * 60 { 0 } else if m < 17 * 60 { 1 } else { 2 };
    Some(day * 3 + k)
}

/// Where `job` is done.
fn job_place(job: &str) -> &'static str {
    park::JOBS.iter().find(|j| j.0 == job).map_or("park", |j| j.2)
}

fn target(reason: u8) -> &'static str {
    match reason {
        MEAL | HUNGRY | WORK => "cafe",
        FARM | FISH | FISHING => "farm",
        _ => "park",
    }
}

impl CharacterOverlay {
    /// On the user's desktop and free to go somewhere.
    fn desk_free(&self, i: usize, windows: &[u64]) -> bool {
        let wk = self.walk_ref(i);
        wk.at_leisure() && wk.trip.is_none() && wk.rest.is_none() && !wk.visiting && !wk.backstage && wk.visit_for == 0 && !windows.contains(&wk.ws)
    }

    /// Sends `i` over to `place` for `reason`.
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

    fn visitors(&self, n: usize, reason: u8) -> usize {
        (0..n).filter(|&i| self.walk_ref(i).visit_for == reason).count()
    }

    /// Every frame in mode 3: keeps the visits going and ends them.
    pub(super) fn visit_tick(&mut self, chats: &mut Vec<Chat>, dt: f32, n: usize) {
        if !visiting_mode() {
            // in other modes nobody is visiting
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
        let fishing = |chats: &Vec<Chat>, i: usize| chats.iter().any(|c| c.stage == Stage::Fishing && c.members.contains(&i));
        let backlog = self.farm_backlog();
        let diners_waiting = (0..n).any(|i| matches!(self.walk_ref(i).visit_for, MEAL | HUNGRY)) || !dine::ORDERS.lock().unwrap().is_empty();
        for i in 0..n {
            let (reason, t, ws, here_t) = {
                let wk = self.walk_ref(i);
                (wk.visit_for, wk.visit_t, wk.ws, wk.here_t)
            };
            let on_windows = windows.contains(&ws);
            let free = self.walk_ref(i).at_leisure() && self.walk_ref(i).trip.is_none() && !in_chat(chats, i);
            // ended up over there for no reason (dropped there, say): home soon
            if reason == 0 {
                if on_windows && free && here_t > 5.0 {
                    let wk = self.walk_of(i);
                    wk.visit_for = DONE;
                    wk.visit_t = 20.0;
                }
                continue;
            }
            // still on the way
            if !on_windows && reason != DONE {
                if t > 60.0 && self.walk_ref(i).trip.is_none() {
                    // never got there (picked up and dropped, say)
                    self.walk_of(i).visit_for = 0;
                }
                continue;
            }
            // there: into the right window if it isn't in it yet
            let place = if reason == ON_SHIFT {
                let name = self.character_name(i);
                park::job_of(&name).or_else(|| park::part_of(&name)).map_or("park", |j| job_place(&j))
            } else {
                target(reason)
            };
            let inside = self.place_here(i).is_some_and(|p| p.place == place);
            if reason != DONE && reason != NO_FOOD && free && !inside && !self.walk_ref(i).social.is_some_and(|s| s.moving) {
                if let Some(p) = governor::place_rect(place) {
                    if self.walk_ref(i).act == Act::Idle {
                        self.walk_into(i, &p);
                    }
                }
            }
            // came to eat: had a look round the cafe and there's nothing
            if matches!(reason, MEAL | HUNGRY) && inside && free && t > 6.0 && !self.food_for(i, false) {
                log::debug!("visit: {i} finds nothing to eat");
                let wk = self.walk_of(i);
                wk.visit_for = NO_FOOD;
                wk.visit_t = 0.0;
                continue;
            }
            let done = match reason {
                // fed (eat() marks it), or nothing to be had
                MEAL | HUNGRY => t > 300.0 && free,
                FARM => (backlog == 0 && free && t > 8.0) || (t > 200.0 && free),
                FISH => {
                    if inside && free && self.walk_ref(i).fish_cool <= 0.0 {
                        let c = self.fish_alone(i, false);
                        chats.push(c);
                        self.walk_of(i).visit_for = FISHING;
                        self.walk_of(i).visit_t = 0.0;
                    }
                    t > 120.0 && free
                }
                FISHING => (!fishing(chats, i) && free && t > 5.0) || (t > 420.0 && free),
                WORK => (!diners_waiting && t > 60.0 && free) || (t > 600.0 && free),
                ATTEND => t > 180.0 && free,
                GAME => t > 45.0 && free,
                // the shift ends when work first does
                ON_SHIFT => !working() && free,
                _ => false,
            };
            if done {
                let wk = self.walk_of(i);
                wk.visit_for = DONE;
                wk.visit_t = 0.0;
            }
            // nothing to eat: straight home for a nap (right here if there's nowhere else)
            if reason == NO_FOOD && free && t > 2.0 {
                let from = self.walk_ref(i).visit_from;
                let wk = self.walk_of(i);
                wk.visit_for = 0;
                match self.desktop_home(from) {
                    Some(home) if home != ws => {
                        // home again; a nap there only if starving
                        let starving = self.walk_ref(i).hunger >= tn().beg_hunger;
                        self.walk_of(i).nap_on_arrival = starving;
                        if !starving {
                            self.walk_of(i).full = self.walk_ref(i).full.max(300.0);
                        }
                        self.send_to(i, home);
                    }
                    _ => self.nothing_to_eat(i),
                }
                continue;
            }
            // done: a little while longer, then home
            if reason == DONE && free && t > 25.0 {
                let from = self.walk_ref(i).visit_from;
                match self.desktop_home(from) {
                    Some(home) if home != ws => {
                        self.send_to(i, home);
                    }
                    _ => {}
                }
                self.walk_of(i).visit_for = 0;
            }
        }
        self.start_visits(chats, n, &windows, backlog);
    }

    /// Eggs, milk and ripe crops waiting on the farm.
    fn farm_backlog(&self) -> u32 {
        let park = park_now();
        let state = park::read_farm_cached();
        park.things.iter().filter(|t| t.place == "farm").map(|t| park::farm_state(t, &state).1).sum()
    }

    /// Who goes over, and why.
    fn start_visits(&mut self, chats: &mut Vec<Chat>, n: usize, windows: &[u64], backlog: u32) {
        let _ = chats;
        let free: Vec<usize> = (0..n).filter(|&i| self.desk_free(i, windows)).collect();
        let away = (0..n).filter(|&i| self.walk_ref(i).visit_for != 0).count();
        let on_desk = (0..n).filter(|&i| !windows.contains(&self.walk_ref(i).ws) && self.walk_ref(i).visit_for == 0).count();
        // only playing: the only visits are game time and the park attendant
        let slot = if playing_only() { None } else { meal_slot() };
        // mealtime: most go to eat, two at a time; three stay behind
        if let Some(slot) = slot {
            // (three stay behind on the desktop)
            if governor::place_rect("cafe").is_some() && on_desk > 3 {
                // only those not full and somewhat hungry
                let go: Vec<usize> = free.iter().copied().filter(|&i| self.walk_ref(i).fed_meal != slot && self.walk_ref(i).full <= 0.0 && self.walk_ref(i).hunger >= tn().hunger_eat * tn().hunger_meal).take(2.min(on_desk - 3)).collect();
                for i in go {
                    self.walk_of(i).fed_meal = slot;
                    self.send_visit(i, "cafe", MEAL);
                }
            }
        }
        // work first: those with a job (full or part-time) to their workplace for the shift
        if working() {
            for i in 0..n {
                let name = self.character_name(i);
                let job = park::job_of(&name).or_else(|| park::part_of(&name));
                if let Some(job) = job.filter(|_| self.desk_free(i, windows)) {
                    self.send_visit(i, job_place(&job), ON_SHIFT);
                }
            }
        }
        // the chef and the waiters, while there are people to feed
        let diners = self.visitors(n, MEAL) + self.visitors(n, HUNGRY);
        if diners > 0 && !playing_only() {
            for job in ["chef", "waiter"] {
                for i in 0..n {
                    if self.is_job(i, job) && self.desk_free(i, windows) {
                        self.send_visit(i, "cafe", WORK);
                    }
                }
            }
        }
        // (no limit on how many are over in the windows at once)
        let _ = away;
        // hungry: to the cafe
        if let Some(&i) = free.iter().find(|&&i| !playing_only() && self.hungry(i)) {
            if governor::place_rect("cafe").is_some() {
                self.send_visit(i, "cafe", HUNGRY);
                return;
            }
        }
        // the farm piling up: its farmer (or whoever farms most) goes
        // (the farmer if there is one; it's already there: nobody else)
        let farmer_there = (0..n).any(|i| self.is_job(i, "farmer") && self.place_here(i).is_some_and(|p| p.place == "farm"));
        if backlog >= 3 && self.visitors(n, FARM) == 0 && !farmer_there && !playing_only() {
            let jobs = park::read_jobs();
            let points = |this: &Self, i: usize| jobs.points.get(&this.character_name(i)).and_then(|p| p.get("farmer")).copied().unwrap_or(0.0);
            let who = free.iter().copied().filter(|&i| self.may_work(i, "farmer")).max_by(|&a, &b| {
                let ka = (self.is_job(a, "farmer") as u8 as f32) * 1000.0 + (self.is_part(a, "farmer") as u8 as f32) * 500.0 + points(self, a);
                let kb = (self.is_job(b, "farmer") as u8 as f32) * 1000.0 + (self.is_part(b, "farmer") as u8 as f32) * 500.0 + points(self, b);
                ka.total_cmp(&kb)
            });
            if let Some(i) = who {
                self.send_visit(i, "farm", FARM);
                return;
            }
        }
        // the fisher, now and then
        static FISH_AT: Mutex<Option<Instant>> = Mutex::new(None);
        static PARK_AT: Mutex<Option<Instant>> = Mutex::new(None);
        let due = |at: &Mutex<Option<Instant>>, every: f32| {
            let mut a = at.lock().unwrap();
            match *a {
                None => {
                    *a = Some(Instant::now());
                    false
                }
                Some(t) if t.elapsed().as_secs_f32() >= every => {
                    *a = Some(Instant::now());
                    true
                }
                _ => false,
            }
        };
        if self.visitors(n, FISH) + self.visitors(n, FISHING) == 0 && !playing_only() && due(&FISH_AT, tn().visit_fish_every) {
            let jobs = park::read_jobs();
            let points = |this: &Self, i: usize| jobs.points.get(&this.character_name(i)).and_then(|p| p.get("fisher")).copied().unwrap_or(0.0);
            let who = free.iter().copied().filter(|&i| self.walk_ref(i).fish_cool <= 0.0 && self.may_work(i, "fisher")).max_by(|&a, &b| {
                let ka = (self.is_job(a, "fisher") as u8 as f32) * 1000.0 + (self.is_part(a, "fisher") as u8 as f32) * 500.0 + points(self, a);
                let kb = (self.is_job(b, "fisher") as u8 as f32) * 1000.0 + (self.is_part(b, "fisher") as u8 as f32) * 500.0 + points(self, b);
                ka.total_cmp(&kb)
            });
            if let Some(i) = who {
                self.send_visit(i, "farm", FISH);
                return;
            }
        }
        // the park attendant looks in on the park
        if due(&PARK_AT, tn().visit_fish_every) {
            if let Some(&i) = free.iter().find(|&&i| self.is_job(i, "attendant")) {
                self.send_visit(i, "park", ATTEND);
            }
        }
    }

    /// Game time in mode 3: a few from the desktop go over to the park for it.
    pub(super) fn game_volunteers(&mut self, n: usize) -> usize {
        let windows: Vec<u64> = governor::places().into_iter().map(|p| p.ws).collect();
        let free: Vec<usize> = (0..n).filter(|&i| self.desk_free(i, &windows) && self.walk_ref(i).energy >= tn().energy_game_need && !self.hungry(i)).collect();
        let on_desk = (0..n).filter(|&i| !windows.contains(&self.walk_ref(i).ws) && self.walk_ref(i).visit_for == 0).count();
        let k = free.len().min(4).min(on_desk.saturating_sub(3));
        for &i in free.iter().take(k) {
            self.send_visit(i, "park", GAME);
        }
        k
    }
}
