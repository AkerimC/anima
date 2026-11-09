//! The desktop's mood rubbing off: dozing off when nobody's around, sitting
//! down when there are too many windows, fretting when the processor is
//! flat out, going to look at new windows. Friendships and rivalries, kept
//! between runs, are here too.

use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;

use crate::backend::desktop;
use crate::render::tuning::get as tn;
use super::CharacterOverlay;
use super::pat::PatCrowd;
use super::walk::{Act, WalkState};

const YAWN: f32 = 1.3;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum RestKind {
    Sleep,
    Sit,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Rest {
    pub(super) kind: RestKind,
    t: f32,
    until: f32,
    // woken: seconds until it's up
    wake: Option<f32>,
    // which way it lies
    turn: f32,
}

impl Rest {
    fn snoring(&self) -> bool {
        self.kind == RestKind::Sleep && self.wake.is_none() && self.t >= YAWN
    }
}

#[derive(Default)]
pub(super) struct Moods {
    idle_t: f32,
    hot_t: f32,
    pub(super) fretting: bool,
    seen_opened: u32,
    seen_closed: u32,
    started: bool,
    // a new window, and how long until its place is known
    pending: Option<(Option<u64>, f32)>,
    pub(super) bonds: Bonds,
    pub(super) bond_tick: f32,
}

impl WalkState {
    pub(super) fn at_leisure(&self) -> bool {
        !self.fret && matches!(self.act, Act::Idle | Act::Walk) && self.social.is_none() && self.race.is_none() && self.crowd.is_none() && !self.circle
    }

    /// `until`: a nap of that many seconds; 0 sleeps until someone's back.
    fn lie_down(&mut self, until: f32) {
        let turn = if self.rand() < 0.5 { FRAC_PI_2 } else { -FRAC_PI_2 };
        self.rest = Some(Rest { kind: RestKind::Sleep, t: 0.0, until, wake: None, turn });
        self.act = Act::Rest;
        self.target = self.x;
    }

    fn sit_down(&mut self) {
        let until = tn().sit_min + self.rand() * tn().sit_rand;
        self.rest = Some(Rest { kind: RestKind::Sit, t: 0.0, until, wake: None, turn: 0.0 });
        self.act = Act::Rest;
        self.target = self.x;
    }

    fn get_up(&mut self) {
        self.rest = None;
        self.act = Act::Idle;
        self.timer = 0.5 + self.rand();
    }

    /// `idle`: nobody touching anything. `drowsy`: for long enough to doze off.
    pub(super) fn rest_tick(&mut self, dt: f32, idle: bool, drowsy: bool, crowded: bool) {
        if self.fret {
            self.sleep_wait = None;
            if self.act == Act::Rest {
                self.get_up();
            }
            return;
        }
        if self.act != Act::Rest {
            self.rest = None;
            if !drowsy {
                self.sleep_wait = None;
            } else if self.at_leisure() {
                let wait = match self.sleep_wait {
                    Some(w) => w - dt,
                    None => self.rand() * tn().sleep_spread,
                };
                self.sleep_wait = Some(wait);
                if wait <= 0.0 {
                    self.sleep_wait = None;
                    self.lie_down(0.0);
                    return;
                }
            }
            // played itself out: its social battery is flat
            self.played = (self.played - dt / 600.0).max(0.0);
            if self.at_leisure() && self.played >= tn().nap_games && self.energy <= tn().nap_energy && self.rand() < dt / tn().nap_wait.max(0.1) {
                self.played = 0.0;
                let until = tn().nap_min + self.rand() * tn().nap_rand;
                self.lie_down(until);
                return;
            }
            if crowded && !drowsy && self.at_leisure() && self.rand() < dt / tn().sit_every.max(1.0) {
                self.sit_down();
            }
            return;
        }
        let Some(mut r) = self.rest else {
            self.act = Act::Idle;
            return;
        };
        r.t += dt;
        match (r.kind, r.wake) {
            (RestKind::Sit, _) => {
                if r.t >= r.until {
                    self.get_up();
                    return;
                }
            }
            (RestKind::Sleep, Some(w)) => {
                if w - dt <= 0.0 {
                    self.get_up();
                    return;
                }
                r.wake = Some(w - dt);
            }
            (RestKind::Sleep, None) if r.until > 0.0 && r.t < r.until => {
                self.energy = (self.energy + dt / r.until).min(1.0);
            }
            (RestKind::Sleep, None) if r.until > 0.0 || !idle => {
                // some jump up with a start, the rest take their time
                if self.rand() < tn().wake_jump {
                    self.get_up();
                    self.start_jump(tn().jump_height * 1.4);
                    return;
                }
                r.wake = Some(tn().wake_min + self.rand() * tn().wake_rand);
                r.t = 0.0;
            }
            _ => {}
        }
        self.rest = Some(r);
    }

    pub(super) fn rest_pose(&mut self, dt: f32) {
        let Some(r) = self.rest else { return };
        let (rot, sink) = match (r.kind, r.wake) {
            (RestKind::Sit, _) => (0.0, tn().sit_sink),
            // rubbing its eyes
            (RestKind::Sleep, Some(_)) => ((r.t * 9.0).sin() * 0.07, 0.0),
            (RestKind::Sleep, None) if r.t < YAWN => ((r.t * 3.5).sin() * 0.1, 0.0),
            (RestKind::Sleep, None) => (r.turn, tn().sleep_sink),
        };
        let k = (dt * 5.0).min(1.0);
        self.rot += ((rot - self.rot + PI).rem_euclid(TAU) - PI) * k;
        self.sink += (sink - self.sink) * k;
        self.feet = false;
    }
}

impl CharacterOverlay {
    pub(super) fn run_moods(&mut self, dt: f32, n: usize) {
        if !self.moods.started {
            self.moods.started = true;
            desktop::spawn();
            self.moods.seen_opened = desktop::OPENED.load(Relaxed);
            self.moods.seen_closed = desktop::CLOSED.load(Relaxed);
        }
        let idle = desktop::IDLE.load(Relaxed);
        self.moods.idle_t = if idle { self.moods.idle_t + dt } else { 0.0 };
        let waited = self.moods.idle_t + desktop::IDLE_AFTER_MS as f32 / 1000.0;
        let drowsy = idle && waited >= tn().sleep_after;
        let crowded = crate::backend::governor::niri_columns().len() >= tn().win_crowded as usize;
        for i in 0..n {
            self.walk_of(i).rest_tick(dt, idle, drowsy, crowded);
        }

        let load = desktop::CPU.load(Relaxed) as f32 / 1000.0;
        if self.moods.fretting {
            if load < tn().cpu_calm {
                self.moods.fretting = false;
                self.moods.hot_t = 0.0;
                for i in 0..n {
                    self.walk_of(i).fret = false;
                }
            }
        } else if load >= tn().cpu_hot {
            self.moods.hot_t += dt;
            if self.moods.hot_t >= tn().cpu_for {
                self.moods.fretting = true;
                // those in a game or a race carry on with it
                for i in 0..n {
                    let wk = self.walk_of(i);
                    wk.crowd = None;
                    wk.fret = wk.rand() < tn().cpu_share;
                }
            }
        } else {
            self.moods.hot_t = 0.0;
        }

        let opened = desktop::OPENED.load(Relaxed);
        if opened != self.moods.seen_opened {
            self.moods.seen_opened = opened;
            let id = *desktop::LAST_OPENED.lock().unwrap();
            // niri's layout for it arrives a moment later
            self.moods.pending = Some((id, 0.5));
        }
        if let Some((id, t)) = self.moods.pending {
            if t - dt > 0.0 {
                self.moods.pending = Some((id, t - dt));
            } else {
                self.moods.pending = None;
                let view = self.view[0] * self.view_w * 0.5;
                let (x, w) = id.and_then(crate::backend::governor::niri_window_x).unwrap_or((view + self.view_w * 0.25, self.view_w * 0.5));
                self.peek_at(x + w * 0.5, w);
            }
        }
        let closed = desktop::CLOSED.load(Relaxed);
        if closed != self.moods.seen_closed {
            self.moods.seen_closed = closed;
            for i in self.few_at_leisure(n) {
                let wk = self.walk_of(i);
                wk.notify_jump = Some(wk.rand() * 0.5);
            }
        }
    }

    /// A few of those with nothing to do, in no particular order.
    fn few_at_leisure(&mut self, n: usize) -> Vec<usize> {
        let here = self.spaces.active;
        let mut free: Vec<usize> = (0..n).filter(|&i| self.walk_ref(i).at_leisure() && (here == 0 || self.walk_ref(i).ws == here)).collect();
        let mut out = Vec::new();
        while !free.is_empty() && out.len() < tn().win_curious as usize {
            let k = ((self.walk.rand() * free.len() as f32) as usize).min(free.len() - 1);
            out.push(free.swap_remove(k));
        }
        out
    }

    /// A new window: a few wander over to have a look.
    fn peek_at(&mut self, center: f32, width: f32) {
        let n = self.extras.len() + 1;
        for i in self.few_at_leisure(n) {
            let spread = width.min(self.view_w * 0.3);
            let from = self.strip_px(i);
            let linger = tn().win_linger;
            let view_w = self.view_w.max(1.0);
            let wk = self.walk_of(i);
            let to = center + (wk.rand() - 0.5) * spread;
            let walk = (to - from) * 2.0 / view_w;
            if walk.abs() <= 1.4 {
                wk.crowd = Some(PatCrowd::curious(wk.x + walk, wk.x, linger, to > center));
            }
        }
    }

    /// Zs over those asleep, sweat on those fretting.
    pub(super) fn mood_props(&self, v: &mut Vec<(i32, [f32; 4], bool, usize)>) {
        let time = self.start.elapsed().as_secs_f32();
        let actors = std::iter::once((&self.walk, self.hit_rect)).chain(self.extras.iter().map(|e| (&e.walk, e.hit_rect)));
        for (k, (wk, r)) in actors.enumerate() {
            let height = r[3] - r[1];
            if height <= 0.0 {
                continue;
            }
            let cx = (r[0] + r[2]) * 0.5;
            let offset = k as f32 * 0.37;
            if wk.act == Act::Rest && wk.rest.is_some_and(|x| x.snoring()) {
                for z in 0..3 {
                    let phase = (time * 0.35 + z as f32 / 3.0 + offset).fract();
                    let size = height * 0.17 * (0.6 + 0.6 * phase) * (1.0 - phase.powi(6));
                    let x = cx + height * (0.12 + 0.22 * phase) + (phase * 7.0).sin() * height * 0.03;
                    let y = r[1] + height * (0.62 - 0.5 * phase);
                    v.push((4, [x - size * 0.5, y - size * 0.5, x + size * 0.5, y + size * 0.5], false, k));
                }
            } else if wk.fret && wk.social.is_none() && wk.race.is_none() && matches!(wk.act, Act::Walk | Act::Idle) {
                let phase = (time * 1.6 + offset).fract();
                let size = height * 0.13 * (1.0 - phase.powi(4));
                let side = if wk.left { 1.0 } else { -1.0 };
                let x = cx + side * height * (0.2 + 0.08 * phase);
                let y = r[1] + height * (0.2 + 0.25 * phase);
                v.push((5, [x - size * 0.5, y - size * 0.5, x + size * 0.5, y + size * 0.5], false, k));
            }
        }
    }

    fn character_name(&self, i: usize) -> String {
        let source = if i == 0 { &self.config.source } else { &self.extras[i - 1].config.source };
        let path = Path::new(source);
        path.parent().and_then(|p| p.file_name()).or(path.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// (friendship, rivalry) of two characters.
    pub(super) fn bond(&self, a: usize, b: usize) -> (f32, f32) {
        self.moods.bonds.get(&self.character_name(a), &self.character_name(b))
    }

    pub(super) fn add_bond(&mut self, a: usize, b: usize, friend: f32, rival: f32) {
        let (x, y) = (self.character_name(a), self.character_name(b));
        self.moods.bonds.add(&x, &y, friend, rival);
    }
}

/// How well each pair of characters knows each other; the file is
/// `bonds.json` in the data folder, by character name so it survives changes
/// of team.
#[derive(Default)]
pub(super) struct Bonds {
    map: HashMap<String, [f32; 2]>,
    loaded: bool,
    dirty: bool,
    save_in: f32,
}

impl Bonds {
    fn key(a: &str, b: &str) -> String {
        if a < b { format!("{a}|{b}") } else { format!("{b}|{a}") }
    }

    fn path() -> std::path::PathBuf {
        crate::util::data_dir().join("bonds.json")
    }

    fn load(&mut self) {
        if !self.loaded {
            self.loaded = true;
            if let Some(map) = std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()) {
                self.map = map;
            }
        }
    }

    pub(super) fn get(&self, a: &str, b: &str) -> (f32, f32) {
        self.map.get(&Self::key(a, b)).map_or((0.0, 0.0), |v| (v[0], v[1]))
    }

    pub(super) fn add(&mut self, a: &str, b: &str, friend: f32, rival: f32) {
        self.load();
        let v = self.map.entry(Self::key(a, b)).or_insert([0.0, 0.0]);
        v[0] = (v[0] + friend).min(3.0);
        v[1] = (v[1] + rival).min(3.0);
        self.dirty = true;
    }

    /// Reads the file once at the start, writes it every couple of minutes.
    pub(super) fn tick(&mut self, dt: f32) {
        self.load();
        self.save_in -= dt;
        if self.dirty && self.save_in <= 0.0 {
            self.save_in = 120.0;
            self.dirty = false;
            if let Ok(text) = serde_json::to_string(&self.map) {
                let _ = crate::util::atomic_write(&Self::path(), text.as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    fn idler() -> WalkState {
        WalkState { act: Act::Idle, timer: 100.0, seed: 11, ..Default::default() }
    }

    #[test]
    fn dozes_off_then_wakes() {
        let mut wk = idler();
        for _ in 0..60 * 30 {
            wk.rest_tick(DT, true, true, false);
            wk.rest_pose(DT);
        }
        assert_eq!(wk.act, Act::Rest);
        assert!(wk.rest.unwrap().snoring());
        assert!((wk.rot.abs() - FRAC_PI_2).abs() < 0.05, "rot {}", wk.rot);
        for _ in 0..60 * 5 {
            wk.rest_tick(DT, false, false, false);
            wk.rest_pose(DT);
            if wk.act != Act::Rest {
                break;
            }
        }
        assert_ne!(wk.act, Act::Rest, "never woke");
        assert!(wk.rest.is_none());
    }

    #[test]
    fn plays_itself_out_and_naps() {
        let mut wk = idler();
        wk.played = 5.0;
        wk.energy = 0.05;
        for _ in 0..60 * 30 {
            wk.rest_tick(DT, false, false, false);
            if wk.act == Act::Rest {
                break;
            }
        }
        assert_eq!(wk.act, Act::Rest, "never napped");
        assert_eq!(wk.played, 0.0);
        let mut frames = 0;
        while wk.act == Act::Rest && frames < 60 * 300 {
            wk.rest_tick(DT, false, false, false);
            frames += 1;
        }
        assert_ne!(wk.act, Act::Rest, "overslept");
        assert!(frames as f32 * DT >= tn().nap_min, "woke after {frames} frames");
        assert!(wk.energy > 0.8, "energy {}", wk.energy);
    }

    #[test]
    fn stays_awake_with_energy_left() {
        let mut wk = idler();
        wk.played = 5.0;
        wk.energy = 0.9;
        for _ in 0..60 * 60 {
            wk.rest_tick(DT, false, false, false);
        }
        assert_ne!(wk.act, Act::Rest);
    }

    #[test]
    fn panics_without_stopping_or_jumping() {
        let mut wk = idler();
        wk.fret = true;
        wk.seed = 21;
        let mut moved = 0.0;
        for _ in 0..60 * 60 {
            let before = wk.x;
            let (_, walking, _) = wk.step(DT, -0.9, 0.9, Some(0.0), 0.0);
            moved += (wk.x - before).abs();
            assert_ne!(wk.act, Act::Jump);
            assert_ne!(wk.act, Act::Chase);
            assert!(walking || wk.act == Act::Walk, "stood still as {:?}", wk.act);
        }
        assert!(moved > 60.0 * tn().walk_speed * 2.0, "only moved {moved}");
    }

    #[test]
    fn panic_gets_it_out_of_bed() {
        let mut wk = idler();
        wk.lie_down(0.0);
        wk.fret = true;
        wk.rest_tick(DT, true, true, false);
        assert_eq!(wk.act, Act::Idle);
    }

    #[test]
    fn stays_awake_if_busy_in_a_chat() {
        let mut wk = idler();
        wk.circle = true;
        for _ in 0..60 * 30 {
            wk.rest_tick(DT, true, true, false);
        }
        assert_ne!(wk.act, Act::Rest);
    }

    #[test]
    fn sits_when_crowded_and_gets_up_again() {
        let mut wk = idler();
        let mut sat = false;
        for _ in 0..60 * 3000 {
            wk.rest_tick(DT, false, false, !sat);
            sat |= wk.act == Act::Rest;
            if sat && wk.act != Act::Rest {
                return;
            }
        }
        panic!("sat {sat}, never got up");
    }

    #[test]
    fn bonds_are_shared_both_ways() {
        let mut b = Bonds { loaded: true, ..Default::default() };
        b.add("Rice", "Suzuka", 0.5, 0.25);
        assert_eq!(b.get("Suzuka", "Rice"), (0.5, 0.25));
        b.add("Suzuka", "Rice", 10.0, 0.0);
        assert_eq!(b.get("Rice", "Suzuka").0, 3.0);
    }
}
