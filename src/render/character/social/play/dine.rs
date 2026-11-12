//! The cafe as a restaurant, once it has a chef: whoever is hungry sits at a
//! table and waits for the waiter (or, with no waiter, queues at the chef's
//! counter and carries the dish over), eats slowly while chatting with whoever
//! sits across, then pays in carrot coins: most to the chef, the rest to the
//! waiter. With no chef, they cook for themselves, free, as before.

use super::*;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Ask {
    // sitting, waiting for the waiter to come over
    Asked,
    // with the chef
    Waiting,
    Cooking,
    // made: on the counter (or the stove) for the diner to fetch
    Ready,
    // on the table
    Served,
    Failed,
}

#[derive(Clone, Debug)]
pub(super) struct Order {
    pub(super) diner: usize,
    pub(super) table: usize,
    pub(super) by_waiter: bool,
    pub(super) state: Ask,
    pub(super) dish: String,
    pub(super) waiter: Option<usize>,
    // who cooked it (gets the cook's share)
    pub(super) cook: Option<usize>,
}

pub(super) static ORDERS: Mutex<Vec<Order>> = Mutex::new(Vec::new());

/// Orders the chef has cooked in a row (a helper takes a turn after two).
static IN_A_ROW: Mutex<u32> = Mutex::new(0);

/// A stove's "diner" when the chef cooks for the counter, not for an order.
pub(super) const PREP: usize = usize::MAX - 1;

fn order_of(diner: usize) -> Option<Order> {
    ORDERS.lock().unwrap().iter().find(|o| o.diner == diner).cloned()
}

pub(super) fn set_order(diner: usize, f: impl FnOnce(&mut Order)) {
    if let Some(o) = ORDERS.lock().unwrap().iter_mut().find(|o| o.diner == diner) {
        f(o);
    }
}

/// Who sits where: (table, place at it, who). One list for every table, so
/// nobody is ever given a chair that's someone else's.
static SEATS: Mutex<Vec<(usize, usize, usize)>> = Mutex::new(Vec::new());

/// `i`'s place at table `k` (of `len`): the one it has, else the first free.
pub(super) fn seat_slot(k: usize, i: usize, len: usize) -> Option<usize> {
    let mut seats = SEATS.lock().unwrap();
    if let Some(s) = seats.iter().find(|s| s.0 == k && s.2 == i) {
        return (s.1 < len).then_some(s.1);
    }
    // a place somewhere else is given up
    seats.retain(|s| s.2 != i);
    let free = (0..len).find(|&p| !seats.iter().any(|s| s.0 == k && s.1 == p))?;
    seats.push((k, free, i));
    Some(free)
}

/// How long a dish takes to eat, slowly, talking.
const EAT: f32 = 30.0;



impl CharacterOverlay {
    /// Someone with that job, in the cafe and free (or doing just that).
    /// Who's working `job` in the cafe: the one with the job, else someone
    /// helping out with it part-time. With no waiter about, a cook helping
    /// out part-time waits on the tables (not the one at the stove).
    pub(super) fn on_duty(&self, job: &str) -> Option<usize> {
        let n = self.extras.len() + 1;
        let here = |i: usize| self.place_here(i).is_some_and(|p| p.place == "cafe") && !self.walk_ref(i).on_break;
        let found = (0..n).find(|&i| self.is_job(i, job) && here(i)).or_else(|| (0..n).find(|&i| self.is_part(i, job) && here(i)));
        if found.is_some() || job != "waiter" {
            return found;
        }
        let chef = self.on_duty("chef");
        (0..n).find(|&i| self.is_part(i, "chef") && here(i) && Some(i) != chef)
    }

    /// Who cooks the next order: the chef, but after two in a row a cook
    /// helping out part-time takes a turn (and does whenever the chef is
    /// busy, eating say).
    fn next_cook(&self) -> Option<usize> {
        let n = self.extras.len() + 1;
        let here = |i: usize| self.place_here(i).is_some_and(|p| p.place == "cafe") && !self.walk_ref(i).on_break;
        let free = |i: usize| self.walk_ref(i).at_leisure() && self.walk_ref(i).act == Act::Idle;
        let chef = (0..n).find(|&i| self.is_job(i, "chef") && here(i));
        let helper = (0..n).find(|&i| self.is_part(i, "chef") && here(i) && free(i));
        let turns = *IN_A_ROW.lock().unwrap();
        match (chef, helper) {
            (Some(c), Some(h)) if turns >= 2 || !free(c) => Some(h),
            (Some(c), _) => Some(c),
            (None, h) => h.or_else(|| self.on_duty("chef")),
        }
    }

    /// The chef is in: the cafe works as a restaurant.
    pub(super) fn restaurant_open(&self) -> bool {
        self.on_duty("chef").is_some()
    }

    /// Where dishes are fetched and paid for: the serving counter, or the
    /// stove nearest the middle of the cafe. (strip px, counter id or 0)
    pub(super) fn pickup(&self) -> Option<(f32, u64)> {
        let park = park_now();
        let things: Vec<&Thing> = park.things.iter().filter(|t| t.place == "cafe").collect();
        if let Some(c) = things.iter().find(|t| t.kind == "counter") {
            return Some((self.thing_px(c), c.id));
        }
        things.iter().find(|t| t.kind == "stove").map(|s| (self.thing_px(s), 0))
    }

    /// Hungry, in the cafe with the chef in: a seat at a table with room.
    pub(super) fn maybe_dine(&mut self, chats: &mut Vec<Chat>, i: usize) -> bool {
        // not whoever is working there right now
        if !self.restaurant_open() || self.on_duty("chef") == Some(i) || self.on_duty("waiter") == Some(i) || order_of(i).is_some() {
            return false;
        }
        // only the hungry (sooner at mealtimes), and only with something in the pantry
        if !self.hungry(i) {
            return false;
        }
        // no coins: asks nicely for a free one
        if self.broke(i) {
            log::debug!("cafe: {} has no coins and asks for a free meal", self.character_name(i));
            let wk = self.walk_of(i);
            wk.gesture = true;
            wk.start_bob(tn().jump_height * 0.15);
        }
        if !hunger::pantry_spare(2) {
            return false;
        }
        let park = park_now();
        let unit = self.park_unit(self.extras.len() + 1);
        let free = park.things.iter().enumerate().filter(|(_, t)| t.kind == "table" && t.place == "cafe").find(|(k, t)| {
            let used = chats.iter().filter(|c| c.play.as_ref().is_some_and(|p| matches!(p.kind.as_str(), "dine" | "table") && p.thing == *k)).map(|c| c.members.len()).sum::<usize>();
            used < self.table_seats(t, self.thing_px(t), unit).len()
        });
        let Some((k, _)) = free else { return false };
        log::debug!("cafe: {i} comes in to eat");
        self.start_play(chats, i, k, "dine");
        true
    }

    /// The restaurant's side: the waiter goes to take orders, the chef cooks
    /// them, and orders whose diner has gone are dropped.
    pub(super) fn restaurant_tick(&mut self, chats: &mut Vec<Chat>) {
        let diners: Vec<usize> = chats.iter().filter(|c| c.play.as_ref().is_some_and(|p| p.kind == "dine")).flat_map(|c| c.members.clone()).collect();
        // places at tables go back once whoever had them has left the table
        let seated: Vec<(usize, usize)> = chats.iter().filter_map(|c| c.play.as_ref().filter(|p| matches!(p.kind.as_str(), "dine" | "table")).map(|p| (p.thing, c.members.clone()))).flat_map(|(k, m)| m.into_iter().map(move |i| (k, i))).collect();
        SEATS.lock().unwrap().retain(|s| seated.contains(&(s.0, s.2)));
        // a break with no meal to it is over
        for i in 0..self.extras.len() + 1 {
            if self.walk_ref(i).on_break && !diners.contains(&i) {
                self.walk_of(i).on_break = false;
            }
        }
        ORDERS.lock().unwrap().retain(|o| diners.contains(&o.diner) && o.state != Ask::Failed);
        // an order nobody is working on any more (the chef or waiter was
        // picked up, say): back to the chef
        let handled: Vec<usize> = chats.iter().filter_map(|c| c.play.as_ref()).filter(|p| matches!(p.kind.as_str(), "stove" | "serve")).map(|p| p.diner).collect();
        let _ = PREP;
        let queued: Vec<usize> = SERVE.lock().unwrap().iter().filter_map(|s| s.diner).collect();
        for o in ORDERS.lock().unwrap().iter_mut() {
            if o.state == Ask::Cooking && !handled.contains(&o.diner) && !queued.contains(&o.diner) {
                o.state = Ask::Waiting;
            }
        }
        let busy = |chats: &Vec<Chat>, kind: &str| chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.kind == kind));
        // nobody to take orders: they go straight to the chef
        if self.on_duty("waiter").is_none() {
            for o in ORDERS.lock().unwrap().iter_mut().filter(|o| o.state == Ask::Asked) {
                o.state = Ask::Waiting;
            }
        }
        let orders = ORDERS.lock().unwrap().clone();
        // the waiter goes over to whoever has sat down
        if let Some(o) = orders.iter().find(|o| o.state == Ask::Asked) {
            if !busy(chats, "order") {
                if let Some(w) = self.on_duty("waiter").filter(|&w| self.walk_ref(w).at_leisure()) {
                    self.start_play(chats, w, o.table, "order");
                    if let Some(p) = chats.iter_mut().find_map(|c| c.play.as_mut().filter(|p| p.kind == "order" && p.riders.is_empty())) {
                        p.diner = o.diner;
                    }
                }
            }
        }
        // nothing ordered: the chef cooks something for the counter (a few at most)
        if !orders.iter().any(|o| o.state == Ask::Waiting) {
            self.prep_for_counter(chats);
        }
        // the chef cooks the next order
        if let Some(o) = orders.iter().find(|o| o.state == Ask::Waiting) {
            let cooking = chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.kind == "stove" && p.diner != usize::MAX));
            if !cooking {
                if let Some(chef) = self.next_cook() {
                    // whose turn: the chef's count goes up, a helper's turn resets it
                    {
                        let mut row = IN_A_ROW.lock().unwrap();
                        *row = if self.is_job(chef, "chef") { *row + 1 } else { 0 };
                    }
                    // the chef stops whatever else it's doing in the cafe
                    if !self.walk_ref(chef).at_leisure() {
                        let mut in_stove = false;
                        for c in chats.iter_mut() {
                            if c.members.contains(&chef) {
                                in_stove = c.play.as_ref().is_some_and(|p| p.kind == "stove");
                                if !in_stove {
                                    c.members.retain(|&x| x != chef);
                                    self.let_down(chef);
                                    self.leave(chef, c.id);
                                }
                            }
                        }
                        if in_stove {
                            return;
                        }
                    }
                    let park = park_now();
                    let me = self.strip_px(chef);
                    let stove = park
                        .things
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.kind == "stove" && t.place == "cafe")
                        .min_by(|a, b| (self.thing_px(a.1) - me).abs().total_cmp(&(self.thing_px(b.1) - me).abs()))
                        .map(|(k, _)| k);
                    // someone's still at that stove: next time
                    if stove.is_some_and(|k| chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.kind == "stove" && p.thing == k))) {
                        return;
                    }
                    match stove {
                        Some(k) => {
                            self.start_play(chats, chef, k, "stove");
                            if let Some(p) = chats.iter_mut().find_map(|c| c.play.as_mut().filter(|p| p.kind == "stove" && p.thing == k && c.members.first() == Some(&chef))) {
                                p.diner = o.diner;
                            }
                            set_order(o.diner, |o| o.state = Ask::Cooking);
                        }
                        // no stove in the cafe: nothing to be had
                        None => set_order(o.diner, |o| o.state = Ask::Failed),
                    }
                }
            }
        }
    }

    /// The chef with nothing on: something for the serving counter, for
    /// whoever's hungry later (at most three waiting there).
    fn prep_for_counter(&mut self, chats: &mut Vec<Chat>) {
        if playing_only() {
            return;
        }
        let Some(chef) = self.on_duty("chef").filter(|&c| self.walk_ref(c).at_leisure() && self.walk_ref(c).act == Act::Idle) else { return };
        let Some((_, counter)) = self.pickup().filter(|p| p.1 != 0) else { return };
        if LIVE.lock().unwrap().dishes.iter().filter(|d| d.0 == counter).count() >= 3 {
            return;
        }
        if !hunger::pantry_spare(2) {
            return;
        }
        let park = park_now();
        let stove = park.things.iter().enumerate().find(|(k, t)| t.kind == "stove" && t.place == "cafe" && !chats.iter().any(|c| c.play.as_ref().is_some_and(|p| p.kind == "stove" && p.thing == *k)));
        if let Some((k, _)) = stove {
            log::debug!("cafe: the chef {chef} cooks for the counter");
            self.start_play(chats, chef, k, "stove");
            if let Some(p) = chats.iter_mut().rev().find_map(|c| c.play.as_mut().filter(|p| p.kind == "stove" && c.members.first() == Some(&chef))) {
                p.diner = PREP;
            }
        }
    }

    /// The chef has made (or burnt) an order: off to the table with the
    /// waiter, or onto the counter for the diner to fetch. Burnt: again.
    pub(super) fn order_cooked(&mut self, diner: usize, dish: Option<String>, stove_px: f32) {
        let Some(o) = order_of(diner) else { return };
        let Some(dish) = dish else {
            set_order(diner, |o| o.state = Ask::Waiting);
            return;
        };
        let (from, counter) = self.pickup().unwrap_or((stove_px, 0));
        set_order(diner, |o| o.dish = dish.clone());
        if counter != 0 {
            LIVE.lock().unwrap().dishes.push((counter, dish.clone(), 1.0));
        }
        let waiter = self.on_duty("waiter").filter(|_| o.by_waiter);
        match waiter {
            Some(w) => {
                set_order(diner, |o| o.waiter = Some(w));
                SERVE.lock().unwrap().push(Serve { who: w, table: o.table, dish, from, counter, by_cook: false, diner: Some(diner) });
                set_order(diner, |o| o.state = Ask::Cooking);
            }
            None => set_order(diner, |o| o.state = Ask::Ready),
        }
    }

    /// The pantry is bare: the order can't be made.
    pub(super) fn order_failed(&mut self, diner: usize) {
        set_order(diner, |o| o.state = Ask::Failed);
    }

    /// The waiter at the table: a nod, and the order goes to the chef.
    pub(super) fn play_order(&mut self, c: &mut Chat, p: &mut Play, dt: f32) -> bool {
        let _ = c;
        let w = c.members[0];
        if p.riders.is_empty() {
            p.riders.push(Rider { i: w, phase: 0, t: 0.0, n: 0, want: 0 });
        }
        let mut r = p.riders[0];
        r.t += dt;
        let d = p.diner;
        if d == usize::MAX || order_of(d).is_none() {
            return true;
        }
        let unit = self.park_unit(self.extras.len() + 1);
        let at = self.strip_px(d) + if self.strip_px(w) < self.strip_px(d) { -0.6 } else { 0.6 } * unit;
        match r.phase {
            0 => {
                if !self.walk_ref(w).social.is_some_and(|s| s.moving) && (self.strip_px(w) - at).abs() > 8.0 && self.arrived(w) {
                    self.go_facing(w, at, 0.2);
                } else if self.arrived(w) && (self.strip_px(w) - at).abs() <= 8.0 {
                    self.face(w, d);
                    self.walk_of(w).start_bob(tn().jump_height * 0.15);
                    r.phase = 1;
                    r.t = 0.0;
                }
            }
            _ if r.t > 1.2 => {
                set_order(d, |o| {
                    o.state = Ask::Waiting;
                    o.waiter = Some(w);
                });
                self.work(w, "waiter", 0.5);
                return true;
            }
            _ => {}
        }
        p.riders[0] = r;
        r.t > 30.0
    }

    /// Diners at one table: sit (or queue first), wait, eat slowly talking
    /// to each other, pay, go.
    pub(super) fn play_dine(&mut self, c: &mut Chat, p: &mut Play, t: &Thing, mid: f32, unit: f32, dt: f32) -> bool {
        let seats = self.table_seats(t, mid, unit);
        let mut off = Vec::new();
        // no place of its own at this table: it goes (and may try another)
        for i in p.riders.iter().map(|r| r.i).collect::<Vec<_>>() {
            if seat_slot(p.thing, i, seats.len()).is_none() {
                set_order(i, |o| o.state = Ask::Failed);
                self.get_off(c, p, i);
            }
        }
        let waiter = self.on_duty("waiter").is_some();
        let pickup = self.pickup();
        // the queue at the counter, in order
        let queue: Vec<usize> = ORDERS.lock().unwrap().iter().filter(|o| !o.by_waiter && matches!(o.state, Ask::Waiting | Ask::Cooking | Ask::Ready)).map(|o| o.diner).collect();
        for k in 0..p.riders.len() {
            let mut r = p.riders[k];
            let i = r.i;
            let Some((seat, seat_up)) = seat_slot(p.thing, i, seats.len()).map(|s| seats[s]) else { continue };
            r.t += dt;
            let go = |this: &mut Self, x: f32, speed: f32| {
                if !this.walk_ref(i).social.is_some_and(|s| s.moving) && (this.strip_px(i) - x).abs() > 8.0 && this.arrived(i) {
                    this.go_facing(i, x, speed);
                }
                this.arrived(i) && (this.strip_px(i) - x).abs() <= 8.0
            };
            match r.phase {
                0 => {
                    r.phase = if waiter || pickup.is_none() { 10 } else { 20 };
                    r.t = 0.0;
                }
                // sit and wait for the waiter
                10 => {
                    if self.seat_walk(i, seat, seat_up, dt) {
                        self.set_face(i, seat > mid);
                        ORDERS.lock().unwrap().push(Order { diner: i, table: p.thing, by_waiter: true, state: Ask::Asked, dish: String::new(), waiter: None, cook: None });
                        r.phase = 11;
                        r.t = 0.0;
                    }
                }
                11 => match order_of(i).map(|o| o.state) {
                    Some(Ask::Served) => {
                        r.phase = 30;
                        r.t = 0.0;
                    }
                    // the waiter's gone: fetches it itself
                    Some(Ask::Ready) => {
                        r.phase = 20;
                        r.t = 0.0;
                    }
                    Some(Ask::Failed) | None => off.push(i),
                    _ if r.t > 120.0 => off.push(i),
                    _ => {}
                },
                // queue at the counter for the chef
                20 => {
                    let (at, _) = pickup.unwrap_or((mid, 0));
                    let side = if mid < at { -1.0 } else { 1.0 };
                    let place = queue.iter().position(|&d| d == i).unwrap_or(queue.len());
                    let spot = at + side * (0.8 + 0.5 * place as f32) * unit;
                    let there = go(self, spot, 0.14);
                    if there && order_of(i).is_none() {
                        ORDERS.lock().unwrap().push(Order { diner: i, table: p.thing, by_waiter: false, state: Ask::Waiting, dish: String::new(), waiter: None, cook: None });
                    }
                    if there {
                        self.set_face(i, side > 0.0);
                    }
                    match order_of(i).map(|o| o.state) {
                        Some(Ask::Ready) if there => {
                            // takes it off the counter
                            let dish = order_of(i).map(|o| o.dish).unwrap_or_default();
                            if let Some((_, cid)) = pickup.filter(|p| p.1 != 0) {
                                let mut l = LIVE.lock().unwrap();
                                if let Some(k) = l.dishes.iter().position(|d| d.0 == cid && d.1 == dish) {
                                    l.dishes.remove(k);
                                }
                            }
                            self.walk_of(i).start_bob(tn().jump_height * 0.15);
                            r.phase = 21;
                            r.t = 0.0;
                        }
                        Some(Ask::Failed) => off.push(i),
                        _ if r.t > 150.0 => off.push(i),
                        _ => {}
                    }
                }
                // carrying it to the table
                21 => {
                    if self.seat_walk(i, seat, seat_up, dt) {
                        let dish = order_of(i).map(|o| o.dish).unwrap_or_default();
                        LIVE.lock().unwrap().dishes.push((t.id, dish, 1.0));
                        set_order(i, |o| o.state = Ask::Served);
                        self.set_face(i, seat > mid);
                        r.phase = 30;
                        r.t = 0.0;
                    }
                }
                // eating, slowly
                30 => {
                    if self.seat_walk(i, seat, seat_up, dt) {
                        self.set_face(i, seat > mid);
                    }
                    let dish = order_of(i).map(|o| o.dish).unwrap_or_default();
                    let left = (1.0 - r.t / EAT).max(0.0);
                    {
                        let mut l = LIVE.lock().unwrap();
                        if let Some(d) = l.dishes.iter_mut().find(|d| d.0 == t.id && d.1 == dish && (d.2 - left).abs() < 0.2) {
                            d.2 = left;
                        } else if let Some(d) = l.dishes.iter_mut().find(|d| d.0 == t.id && d.1 == dish) {
                            d.2 = left;
                        }
                    }
                    if self.walk.rand() < dt * 0.4 {
                        self.walk_of(i).start_bob(tn().jump_height * 0.06);
                    }
                    if left <= 0.0 {
                        let mut l = LIVE.lock().unwrap();
                        if let Some(k) = l.dishes.iter().position(|d| d.0 == t.id && d.1 == dish && d.2 <= 0.01) {
                            l.dishes.remove(k);
                        }
                        drop(l);
                        self.eat(i, &dish);
                        // those who ate together get on better
                        for o in p.riders.iter().filter(|o| o.i != i && o.phase >= 30).map(|o| o.i).collect::<Vec<_>>() {
                            self.add_bond(i, o, tn().bond_game, 0.0);
                        }
                        self.set_happy(i, true);
                        r.phase = 40;
                        r.t = 0.0;
                    }
                }
                // to the counter to pay
                40 => {
                    let at = pickup.map_or(mid, |p| p.0) + if seat < mid { -0.6 } else { 0.6 } * unit;
                    // down off the chair, over to pay
                    if self.seat_walk(i, at, 0.0, dt) || r.t > 20.0 {
                        let name = self.character_name(i);
                        let o = order_of(i);
                        let dish = o.as_ref().map(|o| o.dish.clone()).unwrap_or_default();
                        let price = park::dish_price(&dish);
                        // the cook's share to whoever cooked it
                        let chef = order_of(i).and_then(|o| o.cook).or_else(|| self.on_duty("chef")).map(|c| self.character_name(c));
                        let waiter = o.and_then(|o| o.waiter).map(|w| self.character_name(w));
                        let to: Vec<(&str, f32)> = match (&chef, &waiter) {
                            (Some(c), Some(w)) => vec![(c.as_str(), 0.6), (w.as_str(), 0.4)],
                            (Some(c), None) => vec![(c.as_str(), 1.0)],
                            (None, Some(w)) => vec![(w.as_str(), 1.0)],
                            (None, None) => Vec::new(),
                        };
                        // now and then a friend at the table treats it
                        let treat = p.riders.iter().map(|o| o.i).filter(|&o| o != i && self.close(i, o)).find(|&o| park::coins(&self.character_name(o)) >= price * 2);
                        let treated = match treat.filter(|_| !to.is_empty() && self.walk.rand() < 0.3) {
                            Some(f) => {
                                let fname = self.character_name(f);
                                let ok = park::pay(&fname, price, &to);
                                if ok {
                                    log::debug!("cafe: {fname} treats {name}");
                                    self.add_bond(i, f, tn().bond_game * 2.0, 0.0);
                                }
                                ok
                            }
                            None => false,
                        };
                        // its favourite: a tip for the chef
                        if park::favorite(&name) == dish {
                            if let Some(c) = &chef {
                                park::pay(&name, 2, &[(c.as_str(), 1.0)]);
                            }
                        }
                        if !treated && !to.is_empty() && !park::pay(&name, price, &to) {
                            // short: a friend at the table pays, and they get closer for it
                            let friend = p.riders.iter().map(|o| o.i).filter(|&o| o != i).find(|&o| park::coins(&self.character_name(o)) >= price);
                            if let Some(f) = friend {
                                let fname = self.character_name(f);
                                park::pay(&fname, price, &to);
                                self.add_bond(i, f, tn().bond_game * 2.0, 0.0);
                            }
                        }
                        log::debug!("cafe: {name} pays {price} for the {dish}");

                        self.walk_of(i).start_bob(tn().jump_height * 0.2);
                        r.phase = 41;
                        r.t = 0.0;
                    }
                }
                41 if r.t > 1.2 => off.push(i),
                _ => {}
            }
            p.riders[k] = r;
        }
        // talking across the table while they eat
        let eating = p.riders.iter().filter(|r| r.phase == 30).count();
        if eating >= 2 {
            self.chatter(c, dt);
        }
        for i in off {
            set_order(i, |o| o.state = Ask::Failed);
            self.get_off(c, p, i);
            self.push_down(i);
        }
        c.members.is_empty()
    }

    /// A coin over whoever is paying, the dish in the hands of a diner
    /// carrying it over.
    pub(super) fn dine_props(&self, v: &mut Vec<(i32, [f32; 4], bool, usize)>) {
        for c in self.chats.iter().filter(|c| c.stage == Stage::Play) {
            let Some(p) = c.play.as_ref().filter(|p| p.kind == "dine") else { continue };
            for r in &p.riders {
                let rect = if r.i == 0 { self.hit_rect } else { self.extras[r.i - 1].hit_rect };
                let h = (rect[3] - rect[1]).max(1.0);
                let x = (rect[0] + rect[2]) * 0.5;
                let kind = match r.phase {
                    21 => order_of(r.i).and_then(|o| park::DISHES.iter().position(|d| d.0 == o.dish)).map(|k| super::super::FOOD + k as i32),
                    41 => Some(super::super::COIN),
                    _ => None,
                };
                let Some(kind) = kind else { continue };
                let size = h * if kind == super::super::COIN { 0.2 } else { 0.3 };
                let y = if kind == super::super::COIN { rect[1] + h * self.actor_ref(r.i).opaque[1] - size * (1.0 + r.t * 0.4) } else { self.hands_y(r.i) };
                v.push((kind, [x - size * 0.5, y - size * 0.5, x + size * 0.5, y + size * 0.5], false, r.i));
            }
        }
    }
}
