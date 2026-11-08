//! Playing with the characters: picking them up, carrying and throwing them,
//! petting, drawn circles, notification hops, clinging to the cursor.

use crate::render::tuning::get as tn;
use super::CharacterOverlay;
use super::walk::{Act, WalkState};
use crate::render::FrameInfo;

impl CharacterOverlay {
    /// Pointer and desktop events, handled by the main character for the
    /// whole team.
    pub(super) fn handle_input(&mut self, info: &FrameInfo) {
        // a notification arrived: everyone hops, one after another
        if self.actor == 0 {
            let n = crate::backend::notify::NOTIFICATIONS.load(std::sync::atomic::Ordering::Relaxed);
            if n != self.notified {
                self.notified = n;
                let free = |w: &WalkState| !w.social.is_some_and(|s| s.no_touch);
                if free(&self.walk) {
                    self.walk.notify_jump = Some(0.0);
                }
                for (i, e) in self.extras.iter_mut().enumerate() {
                    if free(&e.walk) {
                        e.walk.notify_jump = Some(0.12 * (i + 1) as f32);
                    }
                }
            }
        }

        // circle gesture: the main character hands it to a random team member
        if self.actor == 0
            && crate::backend::pointer::CIRCLE_GESTURE.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            // logical px == surface px at scale 1
            let at = crate::backend::pointer::CIRCLE_AT.lock().unwrap().take().map(|c| [c.0, c.1, c.2]);
            // one character answers circles at a time: if one is already on its way, re-aim it
            let busy = |w: &WalkState| w.circle_target.is_some() && (w.circle || w.act == Act::Chase);
            let pick = if busy(&self.walk) {
                0
            } else if let Some(i) = self.extras.iter().position(|e| busy(&e.walk)) {
                i + 1
            } else {
                (self.walk.rand() * (self.extras.len() + 1) as f32) as usize
            };
            let wk = match pick.checked_sub(1).and_then(|i| self.extras.get_mut(i)) {
                Some(e) => &mut e.walk,
                None => &mut self.walk,
            };
            if !wk.social.is_some_and(|s| s.no_touch) {
                wk.circle = true;
                wk.circle_target = at;
            }
        }

        // press and hold on a character: pick it up
        if self.actor == 0 {
            let down = crate::backend::pointer::BUTTON_DOWN.load(std::sync::atomic::Ordering::Relaxed);
            let now = self.start.elapsed().as_secs_f32();
            if !down {
                self.press = None;
            } else if self.press.is_none() {
                let [mx, my] = info.mouse;
                let hit = |a: &CharacterOverlay| {
                    let r = a.hit_rect;
                    mx >= r[0] && mx <= r[2] && my >= r[1] && my <= r[3] && !a.walk.social.is_some_and(|s| s.no_touch)
                };
                let target = (1..=self.extras.len())
                    .rev()
                    .find(|&i| hit(&self.extras[i - 1]))
                    .or_else(|| hit(self).then_some(0));
                // measure the grab point now: the cursor may be far away by pick-up time
                let target = target.map(|i| {
                    let r = if i == 0 { self.hit_rect } else { self.extras[i - 1].hit_rect };
                    let (w, h) = (info.width as f32, info.height as f32);
                    (i, [(mx - (r[0] + r[2]) * 0.5) * 2.0 / w, -(my - (r[1] + r[3]) * 0.5) * 2.0 / h])
                });
                self.press = Some((now, [mx, my], target));
            } else if let Some((t0, at, Some((i, grab)))) = self.press {
                // held still for a moment, or dragged a few pixels: pick it up
                let [mx, my] = info.mouse;
                let moved = (mx - at[0]).hypot(my - at[1]) > tn().drag_px * info.scale.max(1.0);
                if now - t0 > tn().pick_delay || moved {
                    let a = if i == 0 { &mut *self } else { &mut self.extras[i - 1] };
                    if a.walk.act != Act::Held {
                        // caught mid-jump: keep it at the height it is drawn at
                        if a.walk.act == Act::Jump && a.walk.jump_dur > 0.0 {
                            let t = 1.0 - (a.walk.jump / a.walk.jump_dur).clamp(0.0, 1.0);
                            a.walk.air += 4.0 * t * (1.0 - t) * a.walk.jump_amp;
                            a.walk.jump = 0.0;
                        }
                        a.walk.act = Act::Held;
                        a.walk.grab = Some(grab);
                        a.walk.held_prev = None;
                        a.walk.vx = 0.0;
                        a.walk.vy = 0.0;
                    }
                    self.press = Some((t0, at, None));
                }
            }
        }

        if self.actor == 0 {
            let carried = |w: &WalkState| w.act == Act::Held && w.hang <= 0.0;
            let held = carried(&self.walk) || self.extras.iter().any(|e| carried(&e.walk));
            crate::backend::pointer::DRAGGING.store(held, std::sync::atomic::Ordering::Relaxed);
            let follows = |w: &WalkState| {
                matches!(w.act, Act::Held | Act::Chase) || w.catch_jump || w.circle || w.social.is_some_and(|s| s.chasing)
            };
            let wanted = follows(&self.walk) || self.extras.iter().any(|e| follows(&e.walk));
            crate::backend::pointer::CURSOR_WANTED.store(wanted, std::sync::atomic::Ordering::Relaxed);
        }

        // clicking on a character that is being petted keeps the petting going
        if self.actor == 0 {
            let n = crate::backend::pointer::CLICKS.load(std::sync::atomic::Ordering::Relaxed);
            if n != self.clicks {
                self.clicks = n;
                let [mx, my] = info.mouse;
                let hit = |r: &[f32; 4]| mx >= r[0] && mx <= r[2] && my >= r[1] && my <= r[3];
                let keep = |a: &mut CharacterOverlay| {
                    if a.walk.act == Act::Pat && hit(&a.hit_rect) {
                        a.walk.timer = a.walk.timer.max(tn().pat_keep);
                        a.patted_again = true;
                    }
                };
                keep(self);
                self.extras.iter_mut().for_each(keep);
            }
        }

        // double click on a character: pet it (topmost one wins)
        if self.actor == 0
            && crate::backend::pointer::DOUBLE_CLICK.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            let [mx, my] = info.mouse;
            let hit = |r: &[f32; 4]| mx >= r[0] && mx <= r[2] && my >= r[1] && my <= r[3];
            log::debug!(
                "pat: mouse ({mx:.0},{my:.0}) main {:?} extras {:?}",
                self.hit_rect.map(|v| v.round()),
                self.extras.iter().map(|e| e.hit_rect.map(|v| v.round())).collect::<Vec<_>>()
            );
            // only on the ground: petting mid-jump or while carried breaks the motion
            let grounded = |w: &WalkState| !matches!(w.act, Act::Jump | Act::Held | Act::Fall) && !w.social.is_some_and(|s| s.no_touch || s.no_pat);
            if let Some(e) = self.extras.iter_mut().rev().find(|e| hit(&e.hit_rect)) {
                if grounded(&e.walk) {
                    e.walk.start_pat();
                }
            } else if hit(&self.hit_rect) && grounded(&self.walk) {
                self.walk.start_pat();
            }
        }
    }

    /// Held: follow the cursor; let go: fall back down (thrown).
    pub(super) fn carry(&mut self, info: &FrameInfo, dt: f32, center_clip: [f32; 2], half_clip: [f32; 2]) {
        let (w, h) = (info.width.max(1) as f32, info.height.max(1) as f32);
        let [view, extra] = self.view;
        let mx = (info.mouse[0] / w) * 2.0 - 1.0;
        let my = 1.0 - (info.mouse[1] / h) * 2.0;
        let down = crate::backend::pointer::BUTTON_DOWN.load(std::sync::atomic::Ordering::Relaxed);
        match self.walk.act {
            Act::Held if down || self.walk.hang > 0.0 => {
                if !down {
                    self.walk.hang -= dt;
                }
                let grab = *self.walk.grab.get_or_insert([
                    mx - (center_clip[0] + self.walk.x - view),
                    my - (center_clip[1] + self.walk.air * half_clip[1]),
                ]);
                let left_room = -(center_clip[0] - half_clip[0] + 1.0) + view;
                let right_room = 1.0 - (center_clip[0] + half_clip[0]) + view;
                self.walk.x = (mx - grab[0] - center_clip[0] + view).clamp(left_room, right_room);
                self.walk.target = self.walk.x;
                self.walk.air = ((my - grab[1] - center_clip[1]) / half_clip[1]).max(0.0);
                // throw speed: smoothed motion while held
                if let Some((px, pa)) = self.walk.held_prev {
                    let k = (dt * 12.0).min(1.0);
                    self.walk.vx += ((self.walk.x - px) / dt - self.walk.vx) * k;
                    self.walk.vy += ((self.walk.air - pa) / dt - self.walk.vy) * k;
                }
                self.walk.held_prev = Some((self.walk.x, self.walk.air));
            }
            Act::Held => {
                self.walk.act = Act::Fall;
                self.walk.held_prev = None;
            }
            Act::Fall => {
                let left_room = -(center_clip[0] - half_clip[0] + 1.0);
                let right_room = 1.0 - (center_clip[0] + half_clip[0]) + extra;
                self.walk.vy -= tn().gravity * dt;
                self.walk.air += self.walk.vy * dt;
                if !self.walk.tag_fall_run(dt) {
                    self.walk.x += self.walk.vx * dt;
                }
                // bounce off the ends of the strip
                if self.walk.x < left_room || self.walk.x > right_room {
                    self.walk.x = self.walk.x.clamp(left_room, right_room);
                    self.walk.vx = -self.walk.vx * tn().wall_bounce;
                }
                // and the top
                let top = (1.0 - center_clip[1]) / half_clip[1] - 1.0;
                if self.walk.air > top {
                    self.walk.air = top;
                    self.walk.vy = -self.walk.vy.abs() * tn().top_bounce;
                }
                self.walk.target = self.walk.x;
                if self.walk.air <= 0.0 {
                    self.walk.air = 0.0;
                    self.walk.vx = 0.0;
                    self.walk.vy = 0.0;
                    self.walk.tumble = 0.0;
                    if self.walk.social.is_some_and(|s| s.flying) {
                        self.walk.plant();
                    } else if self.walk.social.is_some_and(|s| s.no_pat) {
                        // straight back to running, no bounce
                        self.walk.act = Act::Idle;
                        self.walk.timer = 1.0;
                    } else {
                        self.walk.dropped = tn().drop_time;
                        // land with a little bounce
                        self.walk.start_jump(tn().jump_height * tn().land_hop);
                    }
                }
            }
            _ => {}
        }
    }

    /// The catch jump reached the cursor: cling to it by the body for 1-2 s.
    pub(super) fn catch_cling(&mut self, info: &FrameInfo) {
        if self.walk.act == Act::Jump && self.walk.catch_jump {
            let [mx, my] = info.mouse;
            let r = self.hit_rect;
            let size = r[3] - r[1];
            let (hx, hy) = ((r[0] + r[2]) * 0.5, r[1] + size * 0.28);
            let near = size * tn().cling_radius;
            if (mx - hx).powi(2) + (my - hy).powi(2) <= near * near {
                let wk = &mut self.walk;
                let t = 1.0 - (wk.jump / wk.jump_dur.max(0.001)).clamp(0.0, 1.0);
                wk.air += 4.0 * t * (1.0 - t) * wk.jump_amp;
                wk.jump = 0.0;
                wk.catch_jump = false;
                wk.act = Act::Held;
                wk.hang = tn().cling_min + wk.rand() * tn().cling_rand;
                // cursor holds the middle of the body
                wk.grab = Some([0.0, 0.0]);
                wk.held_prev = None;
                wk.vx = 0.0;
                wk.vy = 0.0;
            }
        }
    }

    /// Petting stops as soon as the cursor leaves the character.
    pub(super) fn pat_leaves(&mut self, info: &FrameInfo) {
        let [mx, my] = info.mouse;
        let r = self.hit_rect;
        const M: f32 = 12.0;
        if self.walk.act == Act::Pat
            && !(mx >= r[0] - M && mx <= r[2] + M && my >= r[1] - M && my <= r[3] + M)
        {
            self.walk.start_jump(tn().jump_height * 0.5);
        }
    }
}
