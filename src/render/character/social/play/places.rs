//! In the park, cafe and farm: work first. Whoever has a job goes to it,
//! the farmer collects what's ready, the jobless fish off the pier, and
//! nobody stands about for long.

use super::*;

impl CharacterOverlay {
    /// Eggs in the coop, milk in the barn, a crop ripe: someone on the farm
    /// (a farmer first) goes to get it.
    pub(super) fn farm_calls(&mut self, chats: &mut Vec<Chat>, n: usize) {
        if playing_only() {
            return;
        }
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut next = NEXT.lock().unwrap();
            if next.is_some_and(|t| t.elapsed().as_secs_f32() < 3.0) {
                return;
            }
            *next = Some(Instant::now());
        }
        let Some(farm) = governor::place_rect("farm") else { return };
        let park = park_now();
        let state = park::read_farm_cached();
        let waiting: Vec<(usize, String, f32)> = park
            .things
            .iter()
            .enumerate()
            .filter(|(_, t)| t.place == "farm" && park::farm_state(t, &state).1 > 0)
            .filter(|(k, t)| !chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.thing == *k && p.kind == t.kind)))
            .map(|(k, t)| (k, t.kind.clone(), self.thing_px(t)))
            .collect();
        let Some((k, kind, at)) = waiting.first().cloned() else { return };
        // free on the farm's workspace (in any of the windows: the farmer
        // comes over from wherever it is)
        let free = |this: &Self, i: usize| {
            this.walk_ref(i).ws == farm.ws && this.free_for(i, Want::Work) && this.place_here(i).is_some()
        };
        let who = self.who_for("farmer", at, n, &free);
        if let Some(i) = who {
            log::debug!("farm: {i} goes to collect from the {kind}");
            self.start_play(chats, i, k, &kind);
        }
    }
    /// Work first: those with a job go straight to where it's done; the idle
    /// ones without a job fish off the pier (at least two at it), the fisher
    /// (or a part-time one) first.
    pub(super) fn work_calls(&mut self, chats: &mut Vec<Chat>, n: usize) {
        if !working() {
            return;
        }
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut next = NEXT.lock().unwrap();
            if next.is_some_and(|t| t.elapsed().as_secs_f32() < 3.0) {
                return;
            }
            *next = Some(Instant::now());
        }
        let places = governor::places();
        let free = |this: &Self, i: usize| {
            places.iter().any(|p| p.ws == this.walk_ref(i).ws) && this.free_for(i, Want::Work)
        };
        // to work: one at a time, whoever isn't at its workplace
        for i in 0..n {
            if !free(self, i) {
                continue;
            }
            let name = self.character_name(i);
            let Some(job) = park::job_of(&name).or_else(|| park::part_of(&name)) else { continue };
            let at = park::JOBS.iter().find(|j| j.0 == job).map_or("park", |j| j.2);
            if self.place_here(i).is_some_and(|p| p.place == at) {
                continue;
            }
            if let Some(p) = places.iter().find(|p| p.place == at).filter(|p| p.ws == self.walk_ref(i).ws) {
                log::debug!("work: {i} goes to its job at the {at}");
                let p = p.clone();
                self.walk_into(i, &p);
                return;
            }
        }
        // the pier: at least two fishing
        if governor::place_rect("farm").is_none() {
            return;
        }
        let fishing = chats.iter().filter(|c| c.stage == Stage::Fishing).flat_map(|c| c.members.iter()).filter(|&&i| self.pier_near(i).is_some()).count();
        // the pantry running low: one more out fishing
        let low = park::read_pantry().values().sum::<u32>() < 4 + tn().staff_keep as u32;
        if fishing >= if low { 3 } else { 2 } {
            return;
        }
        let fisher = |this: &Self, i: usize| this.is_job(i, "fisher") || this.is_part(i, "fisher");
        let who = (0..n)
            .filter(|&i| free(self, i) && self.walk_ref(i).fish_cool <= 0.0 && self.may_work(i, "fisher") && !self.hungry(i))
            .filter(|&i| {
                // the jobless, or the fisher; not someone with another job to do
                let name = self.character_name(i);
                fisher(self, i) || (park::job_of(&name).is_none() && park::part_of(&name).is_none())
            })
            .max_by_key(|&i| fisher(self, i) as u8);
        if let Some(i) = who {
            log::debug!("work: {i} goes fishing off the pier");
            let c = self.fish_alone(i, false);
            chats.push(c);
        }
    }
    /// Nobody stands about: after a while doing nothing in the park, cafe or
    /// farm, work first sends the one with a job to do it (the farmer tends
    /// the beds, the chef cooks for the counter) and the one without to fish;
    /// otherwise it finds something to play on, or wanders over to the park.
    pub(super) fn idle_nudge(&mut self, chats: &mut Vec<Chat>, dt: f32, n: usize) {
        let places = governor::places();
        if places.is_empty() {
            return;
        }
        let mut due = Vec::new();
        for i in 0..n {
            let idle = {
                let wk = self.walk_ref(i);
                places.iter().any(|p| p.ws == wk.ws) && wk.social.is_none() && wk.act == Act::Idle && wk.trip.is_none() && wk.rest.is_none() && wk.air <= 0.0
            };
            let wk = self.walk_of(i);
            wk.idle_t = if idle { wk.idle_t + dt } else { 0.0 };
            if wk.idle_t >= tn().idle_nudge {
                wk.idle_t = 0.0;
                due.push(i);
            }
        }
        for i in due {
            let name = self.character_name(i);
            let job = park::job_of(&name).or_else(|| park::part_of(&name));
            let here = self.place_here(i).map(|p| p.place);
            if working() {
                match job.as_deref() {
                    // the farmer: the nearest bed, coop or barn (tended if nothing's ready)
                    Some("farmer") if here.as_deref() == Some("farm") => {
                        let park = park_now();
                        let me = self.strip_px(i);
                        let k = park
                            .things
                            .iter()
                            .enumerate()
                            .filter(|(k, t)| t.place == "farm" && matches!(t.kind.as_str(), "carrots" | "wheat" | "apple_tree") && !chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.thing == *k)))
                            .min_by(|a, b| (self.thing_px(a.1) - me).abs().total_cmp(&(self.thing_px(b.1) - me).abs()))
                            .map(|(k, t)| (k, t.kind.clone()));
                        if let Some((k, kind)) = k {
                            log::debug!("idle: {i} tends the {kind}");
                            self.start_play(chats, i, k, &kind);
                            continue;
                        }
                    }
                    // the fisher: fishing
                    Some("fisher") if self.walk_ref(i).fish_cool <= 0.0 && !self.hungry(i) && governor::place_rect("farm").is_some() => {
                        log::debug!("idle: {i} fishes");
                        let c = self.fish_alone(i, false);
                        chats.push(c);
                        continue;
                    }
                    // chef and waiter wait at their places (the chef cooks for the counter, see the restaurant)
                    Some("chef") | Some("waiter") if here.as_deref() == Some("cafe") => continue,
                    // the rest go to their jobs (work_calls), or play below
                    Some(_) => {}
                    // no job: fishing off the pier, if it isn't crowded
                    None => {
                        let at_pier = chats.iter().filter(|c| c.stage == Stage::Fishing).flat_map(|c| c.members.iter()).filter(|&&j| self.pier_near(j).is_some()).count();
                        if at_pier < 6 && self.walk_ref(i).fish_cool <= 0.0 && governor::place_rect("farm").is_some_and(|f| f.ws == self.walk_ref(i).ws) && !self.hungry(i) {
                            log::debug!("idle: {i} goes fishing");
                            let c = self.fish_alone(i, false);
                            chats.push(c);
                            continue;
                        }
                    }
                }
            }
            // something to play on here, or off to the park
            if self.play_for(chats, i, n, true) {
                log::debug!("idle: {i} finds something to do");
                continue;
            }
            if here.as_deref() != Some("park") {
                if let Some(p) = places.iter().find(|p| p.place == "park" && p.ws == self.walk_ref(i).ws).cloned() {
                    log::debug!("idle: {i} wanders over to the park");
                    self.walk_into(i, &p);
                }
            }
        }
    }
}
