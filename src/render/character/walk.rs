//! How the walking characters get about on their own: strolling, idling,
//! hopping, running after the cursor or into a drawn circle.

use super::race::Racer;
use super::social::Social;

/// What the character is doing right now.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub(super) enum Act {
    #[default]
    Idle,
    Walk,
    Jump,
    Chase,
    Pat,
    Held,
    Fall,
}

/// Random behaviour: walk (likely), idle, jump (less likely) and, rarely,
/// run after the cursor and jump once it's caught.
/// `x` is the offset in clip units from the anchored position.
#[derive(Default)]
pub(super) struct WalkState {
    pub(super) x: f32,
    pub(super) target: f32,
    pub(super) left: bool,
    pub(super) seed: u32,
    pub(super) act: Act,
    pub(super) timer: f32,
    pub(super) jump: f32,
    // current jump: peak height in half-heights of the character, and length
    pub(super) jump_amp: f32,
    pub(super) jump_dur: f32,
    // picked to answer a circle gesture
    pub(super) circle: bool,
    // centre and radius of that circle (surface px): run there and jump into it
    pub(super) circle_target: Option<[f32; 3]>,
    // height above the ground while held/falling (half-heights), fall speed
    pub(super) air: f32,
    pub(super) vy: f32,
    // sideways speed after a throw (clip units/s)
    pub(super) vx: f32,
    // last held position, for the throw speed
    pub(super) held_prev: Option<(f32, f32)>,
    // where the cursor grabbed the sprite, relative to its centre (clip)
    pub(super) grab: Option<[f32; 2]>,
    // seconds until a jump queued by a notification
    pub(super) notify_jump: Option<f32>,
    // jumping at the cursor after a chase
    pub(super) catch_jump: bool,
    // hanging on to the cursor: seconds left
    pub(super) hang: f32,
    // dropped at a random spot on the first frame
    pub(super) placed: bool,
    pub(super) race: Option<Racer>,
    pub(super) social: Option<Social>,
    // seconds before it's up for another chat
    pub(super) social_cool: f32,
    // play a gesture now (3D characters)
    pub(super) gesture: bool,
    // animation time not yet given to the 3D model (skipped frames)
    pub(super) model_dt: f32,
}

pub(super) const PAT_DUR: f32 = 1.4;
// each click while petting keeps it going at least this long
pub(super) const PAT_KEEP: f32 = 0.6;

pub(super) const JUMP_DUR: f32 = 0.9;
pub(super) const JUMP_AMP: f32 = 2.0;

impl WalkState {
    pub(super) fn rand(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.seed >> 8) as f32 / (1u32 << 24) as f32
    }

    pub(super) fn pick(&mut self, min: f32, max: f32, can_chase: bool) {
        let r = self.rand();
        if r < 0.30 {
            // short or long stroll in a random direction
            let d = self.rand();
            let dist = if d < 0.4 {
                0.05 + self.rand() * 0.25
            } else if d < 0.8 {
                0.3 + self.rand() * 0.9
            } else {
                // a trip somewhere along niri's strip
                self.rand() * (max - min)
            };
            let mut dir = if self.rand() < 0.5 { -1.0 } else { 1.0 };
            if (self.x + dir * dist).clamp(min, max) == self.x {
                dir = -dir;
            }
            self.target = (self.x + dir * dist).clamp(min, max);
            self.act = Act::Walk;
        } else if r < 0.93 {
            self.act = Act::Idle;
            self.timer = 1.0 + self.rand() * 3.0;
        } else if r < 0.98 || !can_chase {
            self.start_jump(JUMP_AMP);
        } else {
            self.act = Act::Chase;
            self.timer = 6.0; // give up if the cursor can't be reached
        }
    }

    pub(super) fn start_pat(&mut self) {
        self.act = Act::Pat;
        self.timer = PAT_KEEP;
        self.jump = 0.0;
    }

    pub(super) fn start_jump(&mut self, amp: f32) {
        self.catch_jump = false;
        self.act = Act::Jump;
        self.jump_amp = amp;
        // higher jumps hang in the air a bit longer
        self.jump_dur = JUMP_DUR * (amp / JUMP_AMP).sqrt().max(1.0);
        self.jump = self.jump_dur;
    }

    /// Advance one frame. `mouse` is the cursor x relative to the anchor.
    /// Returns (offset, moving, jump height 0..1).
    pub(super) fn step(&mut self, dt: f32, min: f32, max: f32, mouse: Option<f32>, reach: f32) -> (f32, bool, f32) {
        if matches!(self.act, Act::Held | Act::Fall) {
            return (self.x, false, 0.0);
        }
        if let Some(t) = self.notify_jump.as_mut() {
            *t -= dt;
            if *t <= 0.0 {
                self.notify_jump = None;
                if !matches!(self.act, Act::Pat | Act::Jump) {
                    self.start_jump(JUMP_AMP);
                }
            }
        }
        if let Some(r) = self.race_step(dt) {
            return r;
        }
        if let Some(r) = self.social_step(dt) {
            return r;
        }
        let mut moving = false;
        // circle drawn with the pointer: drop everything and chase it
        if std::mem::take(&mut self.circle)
            && mouse.is_some()
            && self.act != Act::Jump
        {
            self.act = Act::Chase;
            self.timer = 6.0;
        }
        match self.act {
            Act::Idle => {
                self.timer -= dt;
                if self.timer <= 0.0 {
                    self.pick(min, max, mouse.is_some());
                }
            }
            Act::Walk | Act::Chase => {
                let chase = self.act == Act::Chase;
                if chase {
                    self.timer -= dt;
                    match mouse {
                        Some(m) => self.target = m.clamp(min, max),
                        None => self.timer = 0.0,
                    }
                }
                let speed = if chase { 0.4 } else { 0.12 };
                let d = self.target - self.x;
                // close enough counts as caught, or a cursor circling fast
                // keeps flipping the target left and right
                let catch = if chase { 0.015 } else { 0.0 };
                if d.abs() <= speed * dt + catch {
                    if !chase {
                        self.x = self.target;
                    }
                    if chase {
                        // caught it: jump up to the cursor, chase is over
                        self.start_jump(reach.max(JUMP_AMP));
                        self.catch_jump = true;
                    } else {
                        self.pick(min, max, mouse.is_some());
                    }
                } else if chase && self.timer <= 0.0 {
                    self.pick(min, max, false);
                } else {
                    // only turn around when the target is clearly on the other side
                    if !chase || d.abs() > 0.1 {
                        self.left = d < 0.0;
                    }
                    self.x += d.signum() * speed * dt;
                    moving = true;
                }
            }
            Act::Jump | Act::Held | Act::Fall => {}
            Act::Pat => {
                self.timer -= dt;
                if self.timer <= 0.0 {
                    // happy little hop afterwards
                    self.start_jump(JUMP_AMP * 0.5);
                }
            }
        }
        let mut height = 0.0;
        if self.act == Act::Jump {
            self.race_hop(dt);
            self.jump -= dt;
            let t = 1.0 - (self.jump / self.jump_dur).max(0.0);
            height = 4.0 * t * (1.0 - t);
            if self.jump <= 0.0 {
                self.act = Act::Idle;
                self.timer = 0.5 + self.rand();
            }
        }
        (self.x, moving, height)
    }
}
