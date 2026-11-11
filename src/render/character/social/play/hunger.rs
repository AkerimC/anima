//! Hunger: work and time make them hungry, food mends it. A hungry one heads
//! for the cafe (to order, or to cook for itself); a starving one is too worn
//! out for games and sits down. Mealtimes (breakfast, lunch, dinner) bring
//! them in sooner. Each has a favourite dish that fills it more and makes it
//! happy. Now and then a meal is eaten as a picnic in the park; the snack
//! stall sells a quick bite for carrot coins; those with a job get paid.

use super::*;

/// Picnics being set up: (who, where on the strip, the dish, since when).
pub(super) static PICNIC: Mutex<Vec<(usize, f32, String, Instant)>> = Mutex::new(Vec::new());

/// Breakfast, lunch or dinner time: hungry sooner.
pub(super) fn mealtime() -> bool {
    let m = super::super::super::weather::minutes_now();
    (8 * 60..9 * 60).contains(&m) || (12 * 60..13 * 60 + 30).contains(&m) || (19 * 60..20 * 60 + 30).contains(&m)
}

impl CharacterOverlay {
    /// Anything to be had: two things in the pantry to cook with, or a dish
    /// left on a serving counter (not someone else's on a table); with
    /// `stall`, also a snack at the stall it can pay for.
    pub(super) fn food_for(&self, i: usize, stall: bool) -> bool {
        if park::read_pantry().values().filter(|&&v| v > 0).count() >= 2 {
            return true;
        }
        let park = park_now();
        let counters: Vec<u64> = park.things.iter().filter(|t| t.kind == "counter").map(|t| t.id).collect();
        if LIVE.lock().unwrap().dishes.iter().any(|d| counters.contains(&d.0)) {
            return true;
        }
        stall && park.things.iter().any(|t| t.kind == "stall") && park::coins(&self.character_name(i)) >= park::SNACK_PRICE
    }

    /// The chef or a waiter at work and hungry: a quick bite in the kitchen,
    /// from the pantry (they can't sit down as diners while on duty).
    pub(super) fn staff_meal(&mut self, i: usize) -> bool {
        let pantry = park::read_pantry();
        let have: Vec<&str> = park::INGREDIENTS.iter().map(|x| x.0).filter(|x| pantry.get(*x).copied().unwrap_or(0) > 0).collect();
        if have.len() < 2 {
            return false;
        }
        // something it knows how to make, else just two things
        let book = park::read_book();
        let known = park::DISHES.iter().find(|d| book.found.contains_key(d.0) && d.2.iter().all(|x| have.contains(x)));
        let (stuff, dish): (Vec<&str>, &str) = match known {
            Some(d) => (d.2.to_vec(), d.0),
            None => (have[..2].to_vec(), "snack"),
        };
        if !park::take_food(&stuff) {
            return false;
        }
        log::debug!("{} has a quick {dish} in the kitchen", self.character_name(i));
        self.eat(i, dish);
        true
    }

    /// Hungry enough to go and eat.
    pub(super) fn hungry(&self, i: usize) -> bool {
        // come to the cafe for a meal: eats whatever its hunger
        if self.walk_ref(i).visit_for == visit::MEAL {
            return true;
        }
        let h = self.walk_ref(i).hunger;
        h >= tn().hunger_eat || (mealtime() && h >= tn().hunger_eat * 0.55)
    }

    /// `i` eats `dish`: less hungry, happy (more so for its favourite).
    pub(super) fn eat(&mut self, i: usize, dish: &str) {
        let name = self.character_name(i);
        let fav = park::favorite(&name) == dish;
        let k = park::fills(dish, fav);
        let wk = self.walk_of(i);
        wk.hunger = (wk.hunger - k).max(0.0);
        wk.energy = (wk.energy + k * 0.5).min(1.0);
        // came over to eat: that's done
        if matches!(wk.visit_for, visit::MEAL | visit::HUNGRY) {
            wk.visit_for = visit::DONE;
            wk.visit_t = 0.0;
        }
        if fav {
            log::debug!("{name} had its favourite, {dish}");
            wk.start_bob(tn().jump_height * 0.35);
            if let Some(r) = wk.social.as_mut() {
                r.happy = true;
            }
        }
    }

    /// Every frame: everyone gets a little hungrier; every couple of seconds
    /// the hungry go to eat, the starving sit down, and wages are paid.
    pub(super) fn hunger_tick(&mut self, chats: &mut Vec<Chat>, dt: f32, n: usize) {
        let homes = governor::places();
        // the desktop only, or only playing: no hunger to see to
        if homes.is_empty() || tn().park_mode < 0.5 || playing_only() {
            return;
        }
        let visiting = visit::visiting_mode();
        let rate = dt / tn().hunger_time.max(60.0);
        for i in 0..n {
            let starving = {
                let wk = self.walk_of(i);
                // living on the desktop, they get hungry there too
                if !visiting && !homes.iter().any(|p| p.ws == wk.ws) {
                    continue;
                }
                wk.hunger = (wk.hunger + rate).min(1.0);
                wk.hunger >= 0.9
            };
            // too worn out for games
            if starving {
                let low = tn().energy_game_need * 0.6;
                let wk = self.walk_of(i);
                wk.energy = wk.energy.min(low);
            }
        }
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut next = NEXT.lock().unwrap();
            if next.is_some_and(|t| t.elapsed().as_secs_f32() < 2.0) {
                return;
            }
            *next = Some(Instant::now());
        }
        self.pay_wages();
        self.start_picnics(chats);
        // one hungry one at a time goes to eat
        // (in mode 3 those on the desktop are sent over by the visits)
        let hungry: Vec<usize> = (0..n)
            .filter(|&i| self.walk_ref(i).at_leisure() && self.walk_ref(i).trip.is_none() && self.hungry(i))
            // in mode 3 only those here to eat, and the chef and waiters at work;
            // anyone else here for something else eats once back home and sent over
            .filter(|&i| {
                !visiting
                    || (homes.iter().any(|p| p.ws == self.walk_ref(i).ws)
                        && (matches!(self.walk_ref(i).visit_for, visit::MEAL | visit::HUNGRY) || (self.walk_ref(i).visit_for == visit::WORK)))
            })
            .collect();
        let Some(&i) = hungry.get((self.walk.rand() * hungry.len().max(1) as f32) as usize % hungry.len().max(1)) else { return };
        let here = self.place_here(i).map(|p| p.place);
        // at work in the cafe: a quick bite in the kitchen
        if here.as_deref() == Some("cafe") && (self.on_duty("chef") == Some(i) || self.on_duty("waiter") == Some(i)) && self.restaurant_open() {
            if !self.staff_meal(i) && self.walk_ref(i).visit_for == 0 {
                let secs = tn().hungry_nap;
                self.walk_of(i).lie_down(secs);
            }
            return;
        }
        // nothing to eat anywhere: off to sleep instead (once it's had a look in the cafe)
        let stall = here.as_deref() == Some("park");
        if !self.food_for(i, stall) && (here.as_deref() == Some("cafe") || governor::place_rect("cafe").is_none()) {
            if self.walk_ref(i).visit_for == 0 {
                log::debug!("{} finds nothing to eat and has a nap", self.character_name(i));
                let secs = tn().hungry_nap;
                self.walk_of(i).lie_down(secs);
            }
            return;
        }
        let park = park_now();
        let name = self.character_name(i);
        // a snack at the stall, if it's in the park with some coins
        let stall = park.things.iter().enumerate().find(|(_, t)| t.kind == "stall").map(|(k, _)| k);
        if here.as_deref() == Some("park") && park::coins(&name) >= park::SNACK_PRICE && self.walk.rand() < 0.4 {
            if let Some(k) = stall {
                log::debug!("{name} buys a snack");
                self.start_play(chats, i, k, "stall");
                return;
            }
        }
        let cafe = governor::place_rect("cafe");
        match (here.as_deref(), cafe) {
            // in the cafe: order from the chef, or cook something
            (Some("cafe"), _) => {
                if self.restaurant_open() {
                    self.maybe_dine(chats, i);
                } else if park::read_pantry().values().filter(|&&v| v > 0).count() >= 2 {
                    // a free stove to cook at, or one with room to wait at (and share)
                    let used = |k: usize| chats.iter().filter(|c| c.play.as_ref().is_some_and(|p| p.thing == k && p.kind == "stove")).map(|c| c.members.len()).sum::<usize>();
                    let stove = park.things.iter().enumerate().filter(|(_, t)| t.kind == "stove" && t.place == "cafe").map(|(k, _)| k).min_by_key(|&k| used(k)).filter(|&k| used(k) < park::room("stove"));
                    if let Some(k) = stove {
                        log::debug!("{name} is hungry and goes to the stove");
                        self.start_play(chats, i, k, "stove");
                    }
                }
            }
            // elsewhere: over to the cafe
            (_, Some(c)) => {
                if self.walk_ref(i).ws == c.ws {
                    log::debug!("{name} is hungry: off to the cafe");
                    self.walk_into(i, &c);
                } else {
                    self.send_to(i, c.ws);
                }
            }
            _ => {}
        }
    }

    /// Those with a job are paid now and then (every 20 minutes).
    fn pay_wages(&mut self) {
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        let mut last = LAST.lock().unwrap();
        if last.is_none() {
            *last = Some(Instant::now());
            return;
        }
        if last.is_some_and(|t| t.elapsed().as_secs_f32() < 1200.0) {
            return;
        }
        *last = Some(Instant::now());
        let jobs = park::read_jobs();
        for name in jobs.jobs.keys() {
            park::earn(name, tn().wage as u32);
        }
        // part-time: half
        for name in jobs.part.keys() {
            park::earn(name, (tn().wage * 0.5).round() as u32);
        }
        log::debug!("wages paid");
    }

    /// A meal to be eaten in the park: everyone for it goes along.
    fn start_picnics(&mut self, chats: &mut Vec<Chat>) {
        let want: Vec<(usize, f32, String, Instant)> = std::mem::take(&mut *PICNIC.lock().unwrap());
        for (i, x, dish, at) in want {
            // still finishing at the stove: next time (for a while)
            if !self.walk_ref(i).at_leisure() {
                if at.elapsed().as_secs_f32() < 15.0 {
                    PICNIC.lock().unwrap().push((i, x, dish, at));
                }
                continue;
            }
            self.start_play(chats, i, usize::MAX, "picnic");
            if let Some(p) = chats.iter_mut().rev().find_map(|c| c.play.as_mut().filter(|p| p.kind == "picnic" && c.members.contains(&i))) {
                if p.meal.is_none() {
                    p.meal = Some(Meal { dish: dish.clone(), new: false, burnt: false, stuff: Vec::new() });
                    p.from = x;
                }
            }
        }
    }

    /// A picnic: over to the blanket, down on it round the food, eating and
    /// talking till it's gone.
    pub(super) fn play_picnic(&mut self, c: &mut Chat, p: &mut Play, dt: f32) -> bool {
        p.t += dt;
        let unit = self.park_unit(self.extras.len() + 1);
        let dish = p.meal.as_ref().map(|m| m.dish.clone()).unwrap_or_default();
        p.riders.retain(|r| c.members.contains(&r.i));
        for &i in &c.members {
            if !p.riders.iter().any(|r| r.i == i) {
                p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 0 });
            }
        }
        let mut seated = 0;
        for k in 0..p.riders.len() {
            let i = p.riders[k].i;
            let side = if k % 2 == 0 { -1.0 } else { 1.0 };
            let spot = p.from + side * (0.55 + 0.45 * (k / 2) as f32) * unit;
            if p.riders[k].phase == 0 {
                if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - spot).abs() > 8.0 && self.arrived(i) {
                    self.go_facing(i, spot, 0.16);
                } else if self.arrived(i) && (self.strip_px(i) - spot).abs() <= 8.0 {
                    p.riders[k].phase = 1;
                    self.set_face(i, side > 0.0);
                }
                continue;
            }
            seated += 1;
            let wk = self.walk_of(i);
            wk.sink = tn().sit_sink;
            if wk.rand() < dt * 0.4 {
                wk.start_bob(tn().jump_height * 0.06);
            }
        }
        // the food goes once they're sitting round it
        if seated > 0 {
            p.stay += dt * 0.02 * seated as f32;
        }
        if seated >= 2 {
            self.chatter(c, dt);
        }
        set_picnic(p.from, &dish, (1.0 - p.stay).max(0.0));
        if p.stay >= 1.0 || p.t > 120.0 {
            let eaters: Vec<usize> = p.riders.iter().filter(|r| r.phase == 1).map(|r| r.i).collect();
            for (a, &x) in eaters.iter().enumerate() {
                self.eat(x, &dish);
                for &y in &eaters[a + 1..] {
                    self.add_bond(x, y, tn().bond_game * 1.5, 0.0);
                }
                self.walk_of(x).sink = 0.0;
                self.walk_of(x).start_jump(tn().jump_height * 0.5);
            }
            clear_picnic(p.from);
            return true;
        }
        false
    }

    /// At the snack stall: pays, gets an ice cream or cookies, eats it there.
    pub(super) fn play_stall(&mut self, c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
        let i = c.members[0];
        if p.riders.is_empty() {
            p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 0 });
        }
        let mut r = p.riders[0];
        r.t += dt;
        let spot = mid + 0.75 * unit;
        match r.phase {
            0 => {
                if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - spot).abs() > 8.0 && self.arrived(i) {
                    self.go_facing(i, spot, 0.16);
                } else if self.arrived(i) && (self.strip_px(i) - spot).abs() <= 8.0 {
                    self.set_face(i, true);
                    let name = self.character_name(i);
                    // the coins go to the chef, if there is one
                    let chef = park::read_jobs().jobs.iter().find(|j| j.1 == "chef").map(|j| j.0.clone());
                    let paid = match &chef {
                        Some(c) => park::pay(&name, park::SNACK_PRICE, &[(c.as_str(), 1.0)]),
                        None => park::pay(&name, park::SNACK_PRICE, &[("Park", 1.0)]),
                    };
                    if !paid {
                        return true;
                    }
                    let snack = park::SNACKS[(self.walk.rand() * park::SNACKS.len() as f32) as usize % park::SNACKS.len()];
                    p.meal = Some(Meal { dish: snack.to_string(), new: false, burnt: false, stuff: Vec::new() });
                    self.walk_of(i).start_bob(tn().jump_height * 0.2);
                    r.phase = 1;
                    r.t = 0.0;
                }
            }
            _ => {
                if self.walk.rand() < dt * 0.6 {
                    self.walk_of(i).start_bob(tn().jump_height * 0.06);
                }
                if r.t > 6.0 {
                    self.eat(i, "snack");
                    self.set_happy(i, true);
                    p.riders[0] = r;
                    return true;
                }
            }
        }
        p.riders[0] = r;
        p.t > 40.0
    }

    /// The snack in the hand of whoever is eating one.
    pub(super) fn snack_props(&self, v: &mut Vec<(i32, [f32; 4], bool, usize)>) {
        for c in self.chats.iter().filter(|c| c.stage == Stage::Play) {
            let Some(p) = c.play.as_ref().filter(|p| p.kind == "stall") else { continue };
            let Some(r) = p.riders.first().filter(|r| r.phase == 1) else { continue };
            let Some(k) = p.meal.as_ref().and_then(|m| park::DISHES.iter().position(|d| d.0 == m.dish)) else { continue };
            let rect = if r.i == 0 { self.hit_rect } else { self.extras[r.i - 1].hit_rect };
            let h = (rect[3] - rect[1]).max(1.0);
            let size = h * 0.24 * (1.0 - r.t / 8.0).max(0.3);
            let side = if self.walk_ref(r.i).left { -1.0 } else { 1.0 };
            let x = (rect[0] + rect[2]) * 0.5 + side * h * 0.18;
            let y = rect[1] + h * 0.5;
            v.push((super::super::FOOD + k as i32, [x - size * 0.5, y - size * 0.5, x + size * 0.5, y + size * 0.5], false, r.i));
        }
    }

    /// How hungry everyone is, for the windows.
    pub(super) fn hunger_list(&self) -> Vec<(String, f32)> {
        (0..self.extras.len() + 1).map(|i| (self.character_name(i), (self.walk_ref(i).hunger * 100.0).round() / 100.0)).collect()
    }
}

fn set_picnic(x: f32, dish: &str, left: f32) {
    let mut l = LIVE.lock().unwrap();
    l.picnics.retain(|p| (p.0 - x).abs() > 1.0);
    l.picnics.push((x, dish.to_string(), left));
}

fn clear_picnic(x: f32) {
    LIVE.lock().unwrap().picnics.retain(|p| (p.0 - x).abs() > 1.0);
}
