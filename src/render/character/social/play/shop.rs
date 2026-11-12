//! Spending coins: the park's stall also sells things to keep. Someone with
//! coins to spare and nothing to do goes shopping: something to wear, a
//! present for a friend, materials for the chest or a new recipe. The coins
//! go into the park's piggy bank, which pays the attendant's prizes and,
//! once full, buys the park something new.

use super::*;

/// The shop ran on carrot coins, which are gone.
const SHOPPING: bool = false;

impl CharacterOverlay {
    /// Now and then: someone with coins goes shopping; a full piggy bank is spent.
    pub(super) fn shop_tick(&mut self, chats: &mut Vec<Chat>, n: usize) {
        // no coins any more: no shopping
        if !SHOPPING {
            let _ = (chats, n);
            return;
        }
        static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
        {
            let mut next = NEXT.lock().unwrap();
            if next.is_some_and(|t| t.elapsed().as_secs_f32() < 8.0) {
                return;
            }
            *next = Some(Instant::now());
        }
        // the piggy bank's full: new things for the park's chest
        let full = tn().bank_full as u32;
        // (only what's well over what it needs to buy the food: 60 at a time)
        if full > 0 && park::coins("Park") >= full && park::pay("Park", 60, &[]) {
            for _ in 0..3 {
                let k = (self.walk.rand() * park::MATERIALS.len() as f32) as usize % park::MATERIALS.len();
                park::add("Park", park::MATERIALS[k].0);
            }
            log::info!("the park's piggy bank is full: new materials in the chest");
            tell("The park's piggy bank is full: new materials in the chest!");
        }
        if playing_only() {
            return;
        }
        let park = park_now();
        let Some(k) = park.things.iter().position(|t| t.kind == "stall") else { return };
        let rich = tn().shop_rich as u32;
        let who: Vec<usize> = (0..n).filter(|&i| self.place_here(i).is_some_and(|p| p.place == "park") && self.free_for(i, Want::Play) && !self.on_shift(i) && park::coins(&self.character_name(i)) >= rich).collect();
        let Some(&i) = who.get((self.walk.rand() * who.len().max(1) as f32) as usize % who.len().max(1)) else { return };
        if self.walk.rand() >= tn().shop_chance {
            return;
        }
        log::debug!("{} goes shopping", self.character_name(i));
        self.start_play(chats, i, k, "stall");
        if let Some(p) = chats.iter_mut().rev().find_map(|c| c.play.as_mut().filter(|p| p.kind == "stall" && c.members.first() == Some(&i))) {
            p.shop = true;
        }
    }

    /// At the stall to buy something: picks, pays into the piggy bank, takes it.
    pub(super) fn play_shop(&mut self, c: &mut Chat, p: &mut Play, mid: f32, unit: f32, dt: f32) -> bool {
        let i = c.members[0];
        if p.riders.is_empty() {
            p.riders.push(Rider { i, phase: 0, t: 0.0, n: 0, want: 0 });
        }
        let mut r = p.riders[0];
        r.t += dt;
        let spot = mid + 0.75 * unit;
        if r.phase == 0 {
            if !self.walk_ref(i).social.is_some_and(|s| s.moving) && (self.strip_px(i) - spot).abs() > 8.0 && self.arrived(i) {
                self.go_facing(i, spot, 0.16);
            } else if self.arrived(i) && (self.strip_px(i) - spot).abs() <= 8.0 {
                self.set_face(i, true);
                let bought = self.buy(i);
                if !bought {
                    return true;
                }
                let wk = self.walk_of(i);
                wk.start_bob(tn().jump_height * 0.3);
                wk.gesture = true;
                self.set_happy(i, true);
                r.phase = 1;
                r.t = 0.0;
            }
        } else if r.t > 3.0 {
            return true;
        }
        p.riders[0] = r;
        p.t > 40.0
    }

    /// Buys one thing it can afford. True if it did.
    fn buy(&mut self, i: usize) -> bool {
        let name = self.character_name(i);
        let coins = park::coins(&name);
        let n = self.extras.len() + 1;
        let roll = self.walk.rand();
        // something to wear it hasn't got
        if roll < 0.4 && coins >= tn().shop_wear as u32 {
            let pick = self.walk.rand();
            if let Some((id, label, _, _)) = super::super::super::wear::unfound(&name, pick).and_then(super::super::super::wear::catalog_entry) {
                if park::pay(&name, tn().shop_wear as u32, &[("Park", 1.0)]) {
                    super::super::super::wear::give(&name, &id);
                    log::info!("{name} buys {label}");
                    tell(&format!("{name} bought {label}"));
                    return true;
                }
            }
        }
        // a present for its best friend
        if roll < 0.65 && coins >= tn().shop_gift as u32 {
            let friend = (0..n).filter(|&j| j != i).max_by(|&a, &b| self.bond(i, a).0.total_cmp(&self.bond(i, b).0)).filter(|&j| self.bond(i, j).0 >= tn().bond_friend);
            if let Some(f) = friend {
                if park::pay(&name, tn().shop_gift as u32, &[("Park", 1.0)]) {
                    let fname = self.character_name(f);
                    let kind = if self.walk.rand() < 0.5 { 15 } else { 16 };
                    self.moods.keepsakes.add(&fname, kind);
                    self.add_bond(i, f, tn().bond_game * 3.0, 0.0);
                    log::info!("{name} buys {fname} a present");
                    tell(&format!("{name} bought {fname} a present"));
                    return true;
                }
            }
        }
        // a new recipe for the book
        if roll < 0.85 && coins >= tn().shop_recipe as u32 {
            let mut book = park::read_book();
            let left: Vec<&str> = park::DISHES.iter().map(|d| d.0).filter(|d| !book.found.contains_key(*d)).collect();
            if !left.is_empty() && park::pay(&name, tn().shop_recipe as u32, &[("Park", 1.0)]) {
                let dish = left[(self.walk.rand() * left.len() as f32) as usize % left.len()];
                book.found.insert(dish.to_string(), park::Found { by: name.clone(), times: 0 });
                park::write_book(&book);
                log::info!("{name} buys the recipe for {dish}");
                tell(&format!("{name} bought a recipe: {}", park::dish_label(dish)));
                return true;
            }
        }
        // materials for the park's chest
        if coins >= tn().shop_material as u32 && park::pay(&name, tn().shop_material as u32, &[("Park", 1.0)]) {
            let k = (self.walk.rand() * park::MATERIALS.len() as f32) as usize % park::MATERIALS.len();
            park::add(&name, park::MATERIALS[k].0);
            log::info!("{name} buys {} for the chest", park::MATERIALS[k].1);
            return true;
        }
        false
    }
}
