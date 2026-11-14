//! Playing with the characters: picking them up, carrying and throwing them,
//! petting, drawn circles, notification hops, clinging to the cursor.

use crate::render::tuning::get as tn;
use super::CharacterOverlay;
use super::walk::{Act, WalkState};
use crate::render::FrameInfo;

/// Where a clinging character holds on, in half-heights above its centre.
pub(super) const GRIP: f32 = 0.5;

impl CharacterOverlay {
    /// Pointer and desktop events, handled by the main character for the
    /// whole team.
    pub(super) fn handle_input(&mut self, info: &FrameInfo) {
        // a notification arrived: everyone hops, one after another
        if self.actor == 0 {
            let n = crate::backend::notify::NOTIFICATIONS.load(std::sync::atomic::Ordering::Relaxed);
            if n != self.notified {
                self.notified = n;
                let free = |w: &WalkState| w.race.is_none() && !w.social.is_some_and(|s| s.no_touch || s.no_pat);
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
                if let Some((_, _, Some((i, _)))) = self.press.take() {
                    let a = if i == 0 { &mut *self } else { &mut self.extras[i - 1] };
                    if a.walk.sign.is_some_and(super::sign::is_hug)
                        && a.walk.act == Act::Idle
                        && a.walk.race.is_none()
                        && !a.walk.social.is_some_and(|s| s.no_touch || s.no_pat)
                        && a.touching()
                    {
                        let mx = a.scene_mouse()[0];
                        let cx = (a.hit_rect[0] + a.hit_rect[2]) * 0.5;
                        a.walk.left = mx < cx;
                        a.walk.start_hug();
                    }
                }
            } else if self.press.is_none() {
                let [mx, my] = info.mouse;
                let hit = |a: &CharacterOverlay| a.touching() && !a.walk.social.is_some_and(|s| s.no_touch);
                let target = self.topmost(hit);
                // measure the grab point now: the cursor may be far away by pick-up time
                let target = target.map(|i| {
                    let a = if i == 0 { &*self } else { &self.extras[i - 1] };
                    (i, a.body_clip(a.scene_mouse()))
                });
                self.press = Some((now, [mx, my], target));
            } else if let Some((t0, at, Some((i, grab)))) = self.press {
                // held still for a moment, or dragged a few pixels: pick it up
                let [mx, my] = info.mouse;
                let moved = (mx - at[0]).hypot(my - at[1]) > tn().drag_px * info.scale.max(1.0);
                if now - t0 > tn().pick_delay || moved {
                    let a = if i == 0 { &mut *self } else { &mut self.extras[i - 1] };
                    if a.walk.act != Act::Held {
                        // caught in flight: it has moved and turned since the press
                        // (or lying where it landed, or still turning: anything not upright)
                        let flying = matches!(a.walk.act, Act::Fall | Act::Planted)
                            || a.walk.rot.abs() > 0.05
                            || a.walk.social.is_some_and(|s| s.flying || s.stuck);
                        a.walk.sink = 0.0;
                        a.walk.race_down = false;
                        let grab = if flying { a.body_clip(a.scene_mouse()) } else { grab };
                        // it may have moved on since the press: the hand closes on
                        // the body all the same, never on the air beside it
                        let grab = a.on_body_clip(grab);
                        // caught mid-jump: keep it at the height it is drawn at
                        if a.walk.act == Act::Jump && a.walk.jump_dur > 0.0 {
                            let t = 1.0 - (a.walk.jump / a.walk.jump_dur).clamp(0.0, 1.0);
                            a.walk.air += a.walk.arc(t) * a.walk.jump_amp;
                            a.walk.jump = 0.0;
                        }
                        a.walk.act = Act::Held;
                        a.walk.petted = 0.0;
                        a.walk.hug = 0.0;
                        a.walk.grab = Some(grab);
                        // carries on from the angle and spin it had
                        let pi = std::f32::consts::PI;
                        a.walk.swing = if flying { (a.walk.rot + pi).rem_euclid(2.0 * pi) - pi } else { 0.0 };
                        a.walk.swing_v = if flying { a.walk.tumble.clamp(-30.0, 30.0) } else { 0.0 };
                        a.walk.tumble = 0.0;
                        a.walk.rolling = false;
                        if flying {
                            a.walk.grip = true;
                        }
                        a.walk.swing_last = None;
                        a.walk.swing_vel = [0.0, 0.0];
                        a.walk.held_prev = None;
                        a.walk.vx = 0.0;
                        a.walk.vy = 0.0;
                        a.walk.shake = 0.0;
                        a.walk.shaken = false;
                        a.walk.jolted = false;
                        a.walk.impact_dir = None;
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
                let keep = |a: &mut CharacterOverlay| {
                    if a.walk.petted > 0.0 && a.touching() {
                        a.walk.petted = tn().pat_distract;
                    }
                    if a.walk.act == Act::Pat && a.walk.hug <= 0.0 && a.touching() {
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
            log::debug!(
                "pat: mouse ({mx:.0},{my:.0}) main {:?} extras {:?}",
                self.hit_rect.map(|v| v.round()),
                self.extras.iter().map(|e| e.hit_rect.map(|v| v.round())).collect::<Vec<_>>()
            );
            // only on the ground: petting mid-jump or while carried breaks the motion
            let grounded = |w: &WalkState| !matches!(w.act, Act::Jump | Act::Held | Act::Fall) && !w.social.is_some_and(|s| s.no_touch || s.no_pat);
            let hit = self.topmost(|a| a.touching());
            if let Some(i) = hit.filter(|&i| !self.dancing(i)) {
                if grounded(self.walk_ref(i)) && self.walk_ref(i).race.is_none() {
                    if self.walk_ref(i).sign.is_some_and(super::sign::is_hug) {
                        self.walk_of(i).start_hug();
                    } else {
                        self.walk_of(i).start_pat();
                        self.gather_for_pat(i);
                    }
                } else if !matches!(self.walk_ref(i).act, Act::Held) {
                    // busy in a game or a race: a happy moment, and it carries on
                    self.walk_of(i).petted = tn().pat_distract;
                }
            }
        }
    }

    /// The point it turns about, px from its centre, y up.
    fn pivot_px(&self) -> [f32; 2] {
        match (self.walk.grip, self.walk.feet, self.walk.grab) {
            (true, _, Some(g)) => [g[0] * self.surface[0] * 0.5, g[1] * self.surface[1] * 0.5],
            (_, true, _) => [0.0, -(self.hit_rect[3] - self.hit_rect[1]) * 0.5],
            _ => [0.0, 0.0],
        }
    }

    /// A screen point as it sits on the upright sprite: px from its centre, y up.
    fn body_offset(&self, m: [f32; 2]) -> [f32; 2] {
        let r = self.hit_rect;
        let p = [m[0] - (r[0] + r[2]) * 0.5, (r[1] + r[3]) * 0.5 - m[1]];
        let piv = self.pivot_px();
        let (s, c) = self.walk.rot.sin_cos();
        let d = [p[0] - piv[0], p[1] - piv[1]];
        [c * d[0] + s * d[1] + piv[0], c * d[1] - s * d[0] + piv[1]]
    }

    /// `body_offset` in clip units, the form a grab point is kept in.
    fn body_clip(&self, m: [f32; 2]) -> [f32; 2] {
        let o = self.body_offset(m);
        [o[0] * 2.0 / self.surface[0].max(1.0), o[1] * 2.0 / self.surface[1].max(1.0)]
    }

    /// A grab point pulled in onto the drawn body.
    fn on_body_clip(&self, g: [f32; 2]) -> [f32; 2] {
        let [half, top, bottom] = self.body_extent();
        let (sx, sy) = (2.0 / self.surface[0].max(1.0), 2.0 / self.surface[1].max(1.0));
        let (mid, tall) = ((top + bottom) * 0.5, (top - bottom) * 0.5);
        [g[0].clamp(-half * 0.7 * sx, half * 0.7 * sx), g[1].clamp((mid - tall * 0.7) * sy, (mid + tall * 0.7) * sy)]
    }

    /// The drawn part of the sprite on an upright box: x half width, top, bottom, px, y up.
    fn body_extent(&self) -> [f32; 3] {
        let (w, h) = (self.hit_rect[2] - self.hit_rect[0], self.hit_rect[3] - self.hit_rect[1]);
        let [half, top, bottom] = self.opaque;
        [half * w, (0.5 - top) * h, (0.5 - bottom) * h]
    }

    /// The cursor as this character's workspace sees it.
    pub(super) fn scene_mouse(&self) -> [f32; 2] {
        self.zoom.map_or(self.screen_mouse, |z| z.to_scene(self.screen_mouse))
    }

    /// Is the cursor on this character, wherever its workspace is drawn?
    pub(super) fn touching(&self) -> bool {
        self.visible && self.on_body(self.scene_mouse())
    }

    /// Is this screen point on the character, not the empty margin of its image?
    pub(super) fn on_body(&self, m: [f32; 2]) -> bool {
        let o = self.body_offset(m);
        let [half, top, bottom] = self.body_extent();
        o[0].abs() <= half && o[1] <= top && o[1] >= bottom
    }

    /// Screen box around the drawn body, however it's turned.
    pub(super) fn body_box(&self) -> [f32; 4] {
        let r = self.hit_rect;
        let (cx, cy) = ((r[0] + r[2]) * 0.5, (r[1] + r[3]) * 0.5);
        let [half, top, bottom] = self.body_extent();
        let piv = self.pivot_px();
        let (s, c) = self.walk.rot.sin_cos();
        let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for (x, y) in [(-half, top), (half, top), (-half, bottom), (half, bottom)] {
            let d = [x - piv[0], y - piv[1]];
            let (px, py) = (cx + piv[0] + c * d[0] - s * d[1], cy - piv[1] - s * d[0] - c * d[1]);
            b = [b[0].min(px), b[1].min(py), b[2].max(px), b[3].max(py)];
        }
        b
    }

    /// Held: follow the cursor; let go: fall back down (thrown).
    pub(super) fn carry(
        &mut self,
        info: &FrameInfo,
        dt: f32,
        center_clip: [f32; 2],
        half_clip: [f32; 2],
        park_h: f32,
    ) {
        let (w, h) = (info.width.max(1) as f32, info.height.max(1) as f32);
        let [view, extra] = self.own_view;
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
                let raw_air = (my - grab[1] - center_clip[1]) / half_clip[1];
                let (air, ground_slide) = apply_vertical_drag(
                    &mut self.walk.park_y,
                    &mut self.walk.park_target_y,
                    raw_air,
                    half_clip[1],
                    h,
                    park_h,
                );
                self.walk.air = air;
                // throw speed: smoothed motion while held
                if let Some((px, pa)) = self.walk.held_prev {
                    let k = (dt * 12.0).min(1.0);
                    self.walk.vx += ((self.walk.x - px) / dt - self.walk.vx) * k;
                    if ground_slide {
                        self.walk.vy = 0.0;
                    } else {
                        self.walk.vy += ((self.walk.air - pa) / dt - self.walk.vy) * k;
                    }
                } else if ground_slide {
                    self.walk.vy = 0.0;
                }
                self.walk.held_prev = Some((self.walk.x, self.walk.air));
                // held up, by hand or hanging on: a pendulum on the grip, swung
                // round it by the cursor
                {
                    let m = info.mouse;
                    let wk = &mut self.walk;
                    if let Some(l) = wk.swing_last {
                        let v = [(m[0] - l[0]) / dt, -(m[1] - l[1]) / dt];
                        let k = (dt * 25.0).min(1.0);
                        let smooth = [wk.swing_vel[0] + (v[0] - wk.swing_vel[0]) * k, wk.swing_vel[1] + (v[1] - wk.swing_vel[1]) * k];
                        let a = [(smooth[0] - wk.swing_vel[0]) / dt, (smooth[1] - wk.swing_vel[1]) / dt];
                        wk.swing_vel = smooth;
                        let (s, c) = wk.swing.sin_cos();
                        let push = -(a[0] * c + a[1] * s) / 120.0 * tn().cling_swing;
                        wk.swing_v = (wk.swing_v + (push - 40.0 * s - 2.0 * wk.swing_v) * dt).clamp(-30.0, 30.0);
                        wk.swing += wk.swing_v * dt;
                    }
                    wk.swing_last = Some(m);
                    wk.grip = true;
                }
            }
            Act::Held => {
                self.walk.act = Act::Fall;
                self.walk.held_prev = None;
                self.walk.dropped_at = Some(self.screen_mouse);
                // let go mid-swing: keeps spinning the same way until it lands,
                // now about its own middle, moved so nothing jumps on screen
                self.walk.tumble = self.walk.swing_v;
                if let (true, Some(g)) = (self.walk.grip, self.walk.grab) {
                    let aspect = w / h;
                    let (s, c) = self.walk.rot.sin_cos();
                    let p = [g[0] * aspect, g[1]];
                    self.walk.x += (p[0] - (c * p[0] - s * p[1])) / aspect;
                    self.walk.air = (self.walk.air + (p[1] - (s * p[0] + c * p[1])) / half_clip[1]).max(0.0);
                    self.walk.grip = false;
                }
                // going to come down from high up: spinning all the way
                let g = tn().gravity;
                let apex = self.walk.air + self.walk.vy.max(0.0).powi(2) / (2.0 * g);
                if self.walk.tumble.abs() < 2.0 && apex >= tn().fall_high && !self.walk.social.is_some_and(|s| s.no_pat) {
                    let dir = if self.walk.vx < 0.0 { 1.0 } else { -1.0 };
                    self.walk.tumble = dir * (8.0 + self.walk.rand() * 6.0);
                }
            }
            Act::Fall => {
                let left_room = -(center_clip[0] - half_clip[0] + 1.0);
                let right_room = 1.0 - (center_clip[0] + half_clip[0]) + extra;
                // under an open umbrella it drifts down slowly
                let floating = self.walk.parasol && self.walk.vy < 0.0;
                let g = if floating { tn().gravity * tn().parasol_fall } else { tn().gravity };
                self.walk.vy -= g * dt;
                if floating {
                    self.walk.vy = self.walk.vy.max(-tn().parasol_speed);
                }
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
                if self.walk.air > top && self.walk.vy > 0.0 {
                    self.walk.air = top;
                    self.walk.vy = -self.walk.vy.abs() * tn().top_bounce;
                }
                self.walk.target = self.walk.x;
                // knocked flying: off the ground again a few times
                if self.walk.air <= 0.0 {
                    if let Some(s) = self.walk.social.as_mut().filter(|s| s.bounces > 0) {
                        s.bounces -= 1;
                        self.walk.air = 0.01;
                        self.walk.vy = self.walk.vy.abs() * tn().gift_bounce;
                        self.walk.vx *= 0.8;
                    }
                }
                if self.walk.air <= 0.0 {
                    let hard_vy = -(2.0 * tn().gravity * tn().fall_high).sqrt();
                    let hard = self.walk.vy <= hard_vy;
                    let thrown = !self.walk.social.is_some_and(|s| s.flying || s.no_pat);
                    let impact = if thrown {
                        landing_impact(self.walk.vx, self.walk.vy, self.walk.tumble, std::mem::take(&mut self.walk.shaken), hard_vy)
                    } else {
                        self.walk.shaken = false;
                        None
                    };
                    if let Some(dir) = impact {
                        self.walk.impact_dir = Some(dir);
                    }
                    self.walk.air = 0.0;
                    self.walk.vx = 0.0;
                    self.walk.vy = 0.0;
                    // rolls on to upright rather than turning back
                    if self.walk.tumble != 0.0 {
                        let tau = std::f32::consts::TAU;
                        let dir = self.walk.tumble.signum();
                        let rot = (self.walk.rot + tau * 0.5).rem_euclid(tau) - tau * 0.5;
                        self.walk.rot = if rot * dir > 0.6 { rot - dir * tau } else { rot };
                        self.walk.rolling = true;
                    }
                    self.walk.tumble = 0.0;
                    if self.walk.social.is_some_and(|s| s.flying) {
                        self.walk.plant();
                    } else if self.walk.social.is_some_and(|s| s.no_pat) {
                        // straight back to running, no bounce
                        self.walk.act = Act::Idle;
                        self.walk.timer = 1.0;
                    } else {
                        self.walk.dropped = tn().drop_time;
                        let leaped = std::mem::take(&mut self.walk.leaped);
                        if (hard && !leaped) || self.walk.race_down {
                            self.walk.stick_in();
                        } else {
                            // land with a little bounce, sometimes a giggle
                            self.walk.start_jump(tn().jump_height * tn().land_hop);
                            if self.walk.social.is_none() && self.walk.rand() < 0.5 {
                                let secs = tn().land_laugh * 0.5;
                                self.walk.laugh(secs);
                            }
                        }
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
                wk.air += wk.arc(t) * wk.jump_amp;
                wk.jump = 0.0;
                wk.catch_jump = false;
                wk.act = Act::Held;
                wk.hang = tn().cling_min + wk.rand() * tn().cling_rand;
                wk.swing = 0.0;
                wk.swing_v = 0.0;
                wk.swing_last = None;
                wk.swing_vel = [0.0, 0.0];
                // held by the upper body: that's the point it swings round
                wk.grab = Some([0.0, (r[3] - r[1]) / info.height.max(1) as f32 * GRIP]);
                wk.held_prev = None;
                wk.vx = 0.0;
                wk.vy = 0.0;
                wk.shake = 0.0;
                wk.shaken = false;
                wk.jolted = false;
                wk.impact_dir = None;
            }
        }
    }

    /// Petting stops as soon as the cursor leaves the character.
    pub(super) fn pat_leaves(&mut self, info: &FrameInfo) {
        let [mx, my] = info.mouse;
        let r = self.hit_rect;
        const M: f32 = 12.0;
        if self.walk.act == Act::Pat
            && self.walk.hug <= 0.0
            && !(mx >= r[0] - M && mx <= r[2] + M && my >= r[1] - M && my <= r[3] + M)
        {
            self.walk.start_jump(tn().jump_height * 0.5);
        }
    }
}

/// Maps a vertical drag delta (`raw_air`) to `(air, ground_slide)`:
/// - `raw_air >= 0.0`: lifts into the air (`air = raw_air`), leaving `park_y` intact.
/// - `raw_air < 0.0` inside Anima Park (`park_h > 0.0`): slides `park_y` (and `park_target_y`)
///   down toward `0.0` with `air = 0.0` and `ground_slide = true`.
/// - `raw_air < 0.0` outside the park: clamps `air` to `0.0`.
fn apply_vertical_drag(
    park_y: &mut f32,
    park_target_y: &mut f32,
    raw_air: f32,
    half_clip_y: f32,
    surface_h: f32,
    park_h: f32,
) -> (f32, bool) {
    if raw_air >= 0.0 {
        return (raw_air, false);
    }
    if park_h > 0.0 {
        let delta_park_y = raw_air * half_clip_y * surface_h / (2.0 * park_h.max(1.0));
        *park_y = (*park_y + delta_park_y).clamp(0.0, super::park::MEADOW);
        *park_target_y = *park_y;
        return (0.0, true);
    }
    (0.0, false)
}

/// Returns an item-scatter direction when a throw/fall ends on ground impact
/// (hard drop, fast sideways slam, heavy spin, or shaken hard while held).
fn landing_impact(vx: f32, vy: f32, tumble: f32, shaken: bool, hard_vy: f32) -> Option<f32> {
    if !shaken && vy > hard_vy && vx.abs() <= 1.35 && tumble.abs() <= 8.5 {
        return None;
    }
    Some(if vx.abs() > 0.1 {
        vx.signum()
    } else if tumble.abs() > 0.5 {
        -tumble.signum()
    } else {
        0.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn park_vertical_drag_lowers_depth_and_preserves_lift() {
        let mut park_y = 0.45;
        let mut park_target_y = 0.60;

        // Dragging down in Anima Park lowers park_y and syncs park_target_y
        let (air, ground_slide) = apply_vertical_drag(
            &mut park_y,
            &mut park_target_y,
            -0.5,
            0.12,
            1000.0,
            600.0,
        );
        assert_eq!(air, 0.0);
        assert!(ground_slide);
        assert!((park_y - 0.40).abs() < 1e-6, "got park_y={park_y}");
        assert_eq!(park_target_y, park_y);

        // Dragging further down clamps cleanly at 0.0
        let (air, ground_slide) = apply_vertical_drag(
            &mut park_y,
            &mut park_target_y,
            -10.0,
            0.12,
            1000.0,
            600.0,
        );
        assert_eq!(air, 0.0);
        assert!(ground_slide);
        assert_eq!(park_y, 0.0);
        assert_eq!(park_target_y, 0.0);

        // Dragging up lifts into the air and leaves park_y / park_target_y unchanged
        park_y = 0.25;
        park_target_y = 0.25;
        let (air, ground_slide) = apply_vertical_drag(
            &mut park_y,
            &mut park_target_y,
            0.8,
            0.12,
            1000.0,
            600.0,
        );
        assert!((air - 0.8).abs() < 1e-6);
        assert!(!ground_slide);
        assert_eq!(park_y, 0.25);
        assert_eq!(park_target_y, 0.25);
    }

    #[test]
    fn landing_impact_fires_on_slam_or_shake_not_gentle_putdown() {
        let hard_vy = -12.0;
        assert_eq!(landing_impact(0.0, -2.0, 0.0, false, hard_vy), None);
        assert_eq!(landing_impact(0.5, -5.0, 3.0, false, hard_vy), None);
        assert_eq!(landing_impact(2.4, -3.0, 0.0, false, hard_vy), Some(1.0));
        assert_eq!(landing_impact(-2.0, -3.0, 0.0, false, hard_vy), Some(-1.0));
        assert_eq!(landing_impact(0.0, -14.0, 6.0, false, hard_vy), Some(-1.0));
        assert_eq!(landing_impact(0.0, -1.0, 0.0, true, hard_vy), Some(0.0));
    }
}
