//! One network read from the raw board with two outputs: how likely each fighter is to win from
//! here, and what each fighter does here. Trained on the search's own conclusions (`aztrain`):
//! the value on who went on to win, the moves on where the search put its visits. It is the
//! search's memory: a kind of position searched once is known the next time it comes up,
//! wherever on the map and whenever in the game, because the inputs describe the position as
//! the fighter stands in it (where the opponent is from here, the walls around both, shots and
//! blasts on their way, every cooldown and status), not the path that led to it.
//!
//! A trunk reads one fighter's view and gives a score `g` and one number per kind of turn.
//! Fighter 0 wins with chance sigmoid(g(0's view) - g(1's view)), so the two fighters' chances
//! add up to one by construction. Fighter x's turns are weighted by the trunk on x's view.
//!
//! A kind of turn is a movement (none, a step in one of 8 directions, a dash in one of 8) with an
//! attack (none, or one of up to three attacks by its place in the rules): 17 x 4 = 68 kinds.
//!
//! The inputs are facts of the board, mostly yes/no, laid out so that "the opponent is two tiles
//! north-east of me" is one input of its own: no input is a judgment of whether something is
//! good. Only the inputs that are not zero are passed on (`Inputs`), about 60 of 635.
//!
//! The walls are given three tiles out around both fighters: a push throws a fighter three
//! tiles, so a wall three tiles behind either is a slam. Networks made before the opponent's
//! third ring was added (611 inputs, walls two tiles out around the opponent) still load and
//! play exactly as they did; `PvNet::widened` gives such a network the new inputs with
//! weights of zero, so it starts from everything it knows and learns the ring on top.

use crate::geom::{add, chebyshev, DIR_VEC, SUB};
use crate::map::PadKind;
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::turn::{AttackIn, DirIn, RawTurn};
use crate::Engine;

/// Per fighter: hp, buff, status, every cooldown, where it stands against the zone.
const FS: usize = 27;
const R_OPP: i32 = 6;
const R_WALL: i32 = 3;
const R_WALL_T: i32 = 2;
const R_SHOT: i32 = 4;
const R_AREA: i32 = 2;
const N_OPP: usize = ((2 * R_OPP + 1) * (2 * R_OPP + 1)) as usize;
const N_WALL: usize = ((2 * R_WALL + 1) * (2 * R_WALL + 1)) as usize;
const N_WALL_T: usize = ((2 * R_WALL_T + 1) * (2 * R_WALL_T + 1)) as usize;
const N_SHOT: usize = ((2 * R_SHOT + 1) * (2 * R_SHOT + 1)) as usize;
const N_AREA: usize = ((2 * R_AREA + 1) * (2 * R_AREA + 1)) as usize;
/// What is on its way to one fighter: the other's shots around it, whether one is on course and
/// how soon, the nearest one's heading, a blast it stands in and its fuse, the nearest blast.
const TH: usize = N_SHOT + 1 + 4 + 8 + 1 + 7 + N_AREA;
const PADS: usize = 4;
const B_OPP: usize = 2 * FS;
const B_OFF: usize = B_OPP + N_OPP;
const B_DIST: usize = B_OFF + 2;
const B_SEE: usize = B_DIST + 13;
/// Which way the opponent lies at any distance: left / level / right, above / level / below,
/// and whether it is farther off sideways, the same, or farther off up and down.
const B_SIDE: usize = B_SEE + 1;
const B_WALL: usize = B_SIDE + 9;
const B_WALL_T: usize = B_WALL + N_WALL;
const B_THREAT: usize = B_WALL_T + N_WALL_T;
const B_PADS: usize = B_THREAT + 2 * TH;
const B_CTX: usize = B_PADS + 8 * PADS;
/// Number of inputs of one view in networks made before the opponent's third ring of walls.
pub const IN_OLD: usize = B_CTX + 3;
/// The walls exactly three tiles from the opponent (the outer ring of the 7 x 7 around it; the
/// inner 5 x 5 is at `B_WALL_T`). They come after every older input, so an older network's
/// weights are the first `IN_OLD` rows of a newer one's.
const B_RING_T: usize = IN_OLD;
const N_RING_T: usize = (8 * R_WALL) as usize;
/// Number of inputs of one view.
pub const IN: usize = B_RING_T + N_RING_T;
/// The most inputs that can be other than zero at once.
pub const CAP: usize = 192;

/// One view's inputs: only those that are not zero.
#[derive(Clone)]
pub struct Inputs {
    pub n: usize,
    /// how many of them, the first, are inputs an `IN_OLD` network has
    pub n0: usize,
    pub idx: [u16; CAP],
    pub val: [f32; CAP],
}

impl Default for Inputs {
    fn default() -> Inputs {
        Inputs { n: 0, n0: 0, idx: [0; CAP], val: [0.0; CAP] }
    }
}

impl Inputs {
    #[inline]
    fn put(&mut self, i: usize, v: f32) {
        if v != 0.0 && self.n < CAP {
            debug_assert!(i < IN);
            self.idx[self.n] = i as u16;
            self.val[self.n] = v;
            self.n += 1;
        }
    }
    #[inline]
    fn on(&mut self, i: usize) {
        self.put(i, 1.0);
    }
    /// All inputs, zeros included (for tests).
    pub fn dense(&self) -> Vec<f32> {
        let mut v = vec![0.0; IN];
        for k in 0..self.n {
            v[self.idx[k] as usize] += self.val[k];
        }
        v
    }
}

fn fighter_inputs(e: &Engine, s: &State, x: usize, b: usize, out: &mut Inputs) {
    let f = &s.fighters[x];
    out.put(b, f.hp.max(0) as f32 / e.rules.max_hp.max(1) as f32);
    let buff = if f.power_until == NO_BUFF { 0 } else { (f.power_until - s.tick).max(0) };
    out.put(b + 1, buff as f32 / 50.0);
    out.put(b + 2, (buff > 0) as u8 as f32);
    out.put(b + 3, (f.state == FState::Idle) as u8 as f32);
    out.put(b + 4, (f.state == FState::Stunned) as u8 as f32);
    out.put(b + 5, (f.state == FState::Recovering || f.state == FState::Dashing) as u8 as f32);
    out.put(b + 6, if f.state == FState::Idle { 0.0 } else { (f.state_end - s.tick).clamp(0, 12) as f32 / 12.0 });
    out.on(b + 7 + f.cd_move.clamp(0, 4) as usize);
    out.put(b + 12, f.cd_dash.clamp(0, 12) as f32 / 12.0);
    out.put(b + 13, (f.cd_dash == 0) as u8 as f32);
    out.put(b + 14, (f.cd_dash > 0 && f.cd_dash <= 2) as u8 as f32);
    for want in 0..3usize {
        let cd = e.rules.attacks.iter().enumerate().find(|(_, a)| matches!((&a.kind, want), (AttackKind::Projectile { .. }, 0) | (AttackKind::Area { .. }, 1) | (AttackKind::Burst { .. }, 2))).map(|(i, a)| (f.cd_attack[i], a.cooldown.max(1)));
        if let Some((c, m)) = cd {
            out.put(b + 15 + want, c.min(m) as f32 / m as f32);
            out.put(b + 18 + want, (c == 0) as u8 as f32);
            out.put(b + 21 + want, (c > 0 && c <= 2) as u8 as f32);
        }
    }
    let far = chebyshev(f.pos, e.map.center);
    out.put(b + 24, far.min(15) as f32 / 15.0);
    out.put(b + 25, (far > s.zone_radius) as u8 as f32);
    out.put(b + 26, (s.zone_radius - far).clamp(-5, 10) as f32 / 10.0);
}

/// What fighter `a` has on its way to the fighter standing at `v`.
fn threat_inputs(e: &Engine, s: &State, a: usize, v: [i32; 2], b: usize, out: &mut Inputs) {
    let side = 2 * R_SHOT + 1;
    let mut soonest: Option<i32> = None;
    let mut nearest: Option<(i32, usize)> = None;
    let mut seen = [false; N_SHOT];
    for (k, p) in s.projectiles().iter().enumerate() {
        if p.owner as usize != a {
            continue;
        }
        let (dx, dy) = (p.pos[0] - v[0], p.pos[1] - v[1]);
        if dx.abs() <= R_SHOT && dy.abs() <= R_SHOT {
            let cell = ((dy + R_SHOT) * side + dx + R_SHOT) as usize;
            if !seen[cell] {
                seen[cell] = true;
                out.on(b + cell);
            }
        }
        let d = dx.abs().max(dy.abs());
        if nearest.map(|n| d < n.0).unwrap_or(true) {
            nearest = Some((d, k));
        }
        // on course: its line passes within half a tile of the fighter, ahead of it, in reach
        let (lx, ly) = ((p.target[0] - p.origin[0]) as f64, (p.target[1] - p.origin[1]) as f64);
        let len = (lx * lx + ly * ly).sqrt();
        if len > 0.0 {
            let (rx, ry) = (v[0] as f64 * SUB as f64 - p.exact_pos[0] as f64, v[1] as f64 * SUB as f64 - p.exact_pos[1] as f64);
            let along = (rx * lx + ry * ly) / len;
            let off = (rx * ly - ry * lx).abs() / len;
            let reach = (p.max_travel - p.traveled) as f64;
            if along >= 0.0 && along <= reach + 0.5 * SUB as f64 && off < 0.5 * SUB as f64 {
                let ticks = (along / (p.speed.max(1) as f64 * SUB as f64)).ceil().max(1.0) as i32;
                soonest = Some(soonest.map(|t| t.min(ticks)).unwrap_or(ticks));
            }
        }
    }
    let mut at = b + N_SHOT;
    if let Some(t) = soonest {
        out.on(at);
        out.on(at + 1 + (t.clamp(1, 4) - 1) as usize);
    }
    at += 5;
    if let Some((_, k)) = nearest {
        let p = &s.projectiles()[k];
        let (lx, ly) = ((p.target[0] - p.origin[0]) as f64, (p.target[1] - p.origin[1]) as f64);
        if lx != 0.0 || ly != 0.0 {
            let o = ((ly.atan2(lx) / std::f64::consts::FRAC_PI_4).round() as i32).rem_euclid(8);
            out.on(at + o as usize);
        }
    }
    at += 8;
    let mut fuse: Option<i32> = None;
    let mut near: Option<(i32, [i32; 2])> = None;
    for ar in s.areas().iter().filter(|ar| ar.owner as usize == a) {
        if e.map.in_blast(ar.pos, ar.radius, v) {
            let t = (ar.explodes_tick - s.tick).clamp(0, 6);
            fuse = Some(fuse.map(|f| f.min(t)).unwrap_or(t));
        }
        let d = chebyshev(ar.pos, v);
        if near.map(|n| d < n.0).unwrap_or(true) {
            near = Some((d, [ar.pos[0] - v[0], ar.pos[1] - v[1]]));
        }
    }
    if let Some(t) = fuse {
        out.on(at);
        out.on(at + 1 + t as usize);
    }
    at += 8;
    if let Some((_, [dx, dy])) = near {
        if dx.abs() <= R_AREA && dy.abs() <= R_AREA {
            out.on(at + ((dy + R_AREA) * (2 * R_AREA + 1) + dx + R_AREA) as usize);
        }
    }
}

/// The board as fighter `x` stands in it.
pub fn encode(e: &Engine, s: &State, x: usize, out: &mut Inputs) {
    out.n = 0;
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    fighter_inputs(e, s, x, 0, out);
    fighter_inputs(e, s, 1 - x, FS, out);
    let (dx, dy) = (o.pos[0] - f.pos[0], o.pos[1] - f.pos[1]);
    if dx.abs() <= R_OPP && dy.abs() <= R_OPP {
        out.on(B_OPP + ((dy + R_OPP) * (2 * R_OPP + 1) + dx + R_OPP) as usize);
    }
    out.put(B_OFF, dx as f32 / 20.0);
    out.put(B_OFF + 1, dy as f32 / 20.0);
    let d = dx.abs().max(dy.abs());
    if d >= 1 {
        out.on(B_DIST + (d.min(13) - 1) as usize);
    }
    out.put(B_SEE, can_see(&e.rules, &e.map, f.pos, o.pos) as u8 as f32);
    out.on(B_SIDE + (dx.signum() + 1) as usize);
    out.on(B_SIDE + 3 + (dy.signum() + 1) as usize);
    out.on(B_SIDE + 6 + ((dx.abs() - dy.abs()).signum() + 1) as usize);
    let wall = |p: [i32; 2]| p[0] < 0 || p[1] < 0 || p[0] >= e.map.w || p[1] >= e.map.h || e.map.is_wall(p);
    for wy in -R_WALL..=R_WALL {
        for wx in -R_WALL..=R_WALL {
            if wall([f.pos[0] + wx, f.pos[1] + wy]) {
                out.on(B_WALL + ((wy + R_WALL) * (2 * R_WALL + 1) + wx + R_WALL) as usize);
            }
        }
    }
    for wy in -R_WALL_T..=R_WALL_T {
        for wx in -R_WALL_T..=R_WALL_T {
            if wall([o.pos[0] + wx, o.pos[1] + wy]) {
                out.on(B_WALL_T + ((wy + R_WALL_T) * (2 * R_WALL_T + 1) + wx + R_WALL_T) as usize);
            }
        }
    }
    threat_inputs(e, s, 1 - x, f.pos, B_THREAT, out);
    threat_inputs(e, s, x, o.pos, B_THREAT + TH, out);
    for i in 0..PADS {
        let Some(pd) = e.map.pads.get(i) else { continue };
        let b = B_PADS + 8 * i;
        out.put(b, (pd.pos[0] - f.pos[0]) as f32 / e.map.w.max(1) as f32);
        out.put(b + 1, (pd.pos[1] - f.pos[1]) as f32 / e.map.h.max(1) as f32);
        out.put(b + 2, if s.pads[i].available { 0.0 } else { (s.pads[i].respawn_tick - s.tick).clamp(0, 200) as f32 / 200.0 });
        out.put(b + 3, if pd.kind == PadKind::Power { 1.0 } else { -1.0 });
        out.put(b + 4, s.pads[i].available as u8 as f32);
        let (mine, theirs) = (chebyshev(pd.pos, f.pos), chebyshev(pd.pos, o.pos));
        out.put(b + 5, mine.min(20) as f32 / 20.0);
        out.put(b + 6, theirs.min(20) as f32 / 20.0);
        out.put(b + 7, (mine < theirs) as u8 as f32);
    }
    out.put(B_CTX, s.tick as f32 / 600.0);
    out.put(B_CTX + 1, s.zone_radius.clamp(0, 15) as f32 / 15.0);
    out.on(B_CTX + 2);
    out.n0 = out.n;
    // the ring three tiles from the opponent, row by row
    let mut k = 0;
    for wy in -R_WALL..=R_WALL {
        for wx in -R_WALL..=R_WALL {
            if wx.abs().max(wy.abs()) == R_WALL {
                if wall([o.pos[0] + wx, o.pos[1] + wy]) {
                    out.on(B_RING_T + k);
                }
                k += 1;
            }
        }
    }
}

pub const MOVES: usize = 17;
pub const ATTACKS: usize = 4;
pub const CLASSES: usize = MOVES * ATTACKS;
/// The widest hidden layer.
pub const MAX_H: usize = 128;

/// The kind of a turn (`CLASSES`).
pub fn class_of(t: &RawTurn) -> usize {
    let m = match (t.dash, t.mv) {
        (DirIn::Dir(d), _) => 9 + (d as usize & 7),
        (_, DirIn::Dir(d)) => 1 + (d as usize & 7),
        _ => 0,
    };
    let a = match t.attack {
        AttackIn::Named { attack, .. } => 1 + (attack as usize).min(ATTACKS - 2),
        _ => 0,
    };
    m * ATTACKS + a
}

/// Every turn fighter `x` may consider at `s`: the flagship search's list (steps that are not
/// blocked, every dash, each ready attack at the opponent, the repulsor also after a dash from
/// farther away), without any guess at which is good.
pub fn turns(e: &Engine, s: &State, x: usize, out: &mut Vec<RawTurn>) {
    out.clear();
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if f.state != FState::Idle || !f.alive || !o.alive {
        out.push(RawTurn::IDLE);
        return;
    }
    let d = chebyshev(f.pos, o.pos);
    let seen = can_see(&e.rules, &e.map, f.pos, o.pos);
    let ready = |want: u8| {
        e.rules.attacks.iter().enumerate().find(|(i, a)| {
            f.cd_attack[*i] == 0
                && match (&a.kind, want) {
                    (AttackKind::Projectile { .. }, 0) => seen,
                    (AttackKind::Area { range, .. }, 1) => seen && d <= *range,
                    (AttackKind::Burst { .. }, 2) => d <= 3,
                    _ => false,
                }
        }).map(|(i, _)| i as u8)
    };
    let (shot, frost, burst) = (ready(0), ready(1), ready(2));
    let burst_far = if burst.is_none() && f.cd_dash == 0 && d <= 3 + e.rules.dash_distance {
        e.rules.attacks.iter().enumerate().find(|(i, a)| f.cd_attack[*i] == 0 && matches!(a.kind, AttackKind::Burst { .. })).map(|(i, _)| i as u8)
    } else {
        None
    };
    let with_attacks = |mv: RawTurn, dash: bool, out: &mut Vec<RawTurn>| {
        out.push(mv);
        if let Some(a) = shot {
            out.push(mv.with_attack(a, o.pos));
        }
        if let Some(a) = burst {
            out.push(mv.with_burst(a));
        } else if let (Some(a), true) = (burst_far, dash) {
            out.push(mv.with_burst(a));
        }
        if let Some(a) = frost {
            out.push(mv.with_attack(a, o.pos));
        }
    };
    with_attacks(RawTurn::IDLE, false, out);
    if f.cd_move == 0 {
        for dir in 0..8u8 {
            let dest = add(f.pos, DIR_VEC[dir as usize]);
            if e.map.step_blocked(f.pos, dir) || dest == o.pos {
                continue;
            }
            with_attacks(RawTurn::step(dir), false, out);
        }
    }
    if f.cd_dash == 0 {
        for dir in 0..8u8 {
            with_attacks(RawTurn::dash(dir), true, out);
        }
    }
}

/// The hidden units' curve: z / (1 + |z|). It flattens toward -1 and 1 but its slope never
/// reaches zero, so a unit pushed far out during training can still come back (with a curve
/// clamped flat, as `rawnet::act` is, two thirds of the second layer went dead in the first
/// epoch and stayed there).
#[inline]
pub fn act(z: f32) -> f32 {
    z / (1.0 + z.abs())
}

/// The slope of `act` at `z`.
#[inline]
pub fn act_d(z: f32) -> f32 {
    let d = 1.0 + z.abs();
    1.0 / (d * d)
}

#[inline(always)]
fn axpy(acc: &mut [f32], x: f32, w: &[f32]) {
    for (a, w) in acc.iter_mut().zip(w.iter()) {
        *a += x * *w;
    }
}

/// One block of a layer: for each of `B` units from `at`, its bias and then every input times
/// its weight to the unit, added in the order the inputs come. The running sums are a local
/// array of a fixed size, so the compiler holds them in registers for the whole block where
/// `axpy` over the layer loads and stores every unit's sum once for each input. A unit's sum is
/// built from the same numbers in the same order either way, so it comes out the same to the
/// last bit.
#[inline(always)]
fn block<'w, const B: usize>(out: &mut [f32], bias: &[f32], at: usize, n: usize, input: &impl Fn(usize) -> (f32, &'w [f32])) {
    let mut acc = [0.0f32; B];
    acc.copy_from_slice(&bias[at..at + B]);
    for k in 0..n {
        let (x, row) = input(k);
        let row: &[f32; B] = row[at..at + B].try_into().unwrap();
        for j in 0..B {
            acc[j] += x * row[j];
        }
    }
    out[at..at + B].copy_from_slice(&acc);
}

/// A whole layer before its units' curve: `out[j] = bias[j] + the sum over the n inputs of
/// input k's value times its weight to unit j`, in blocks of 32 units (then 8, 4 and single
/// ones for what is left).
#[inline(always)]
fn layer<'w>(out: &mut [f32], bias: &[f32], n: usize, input: impl Fn(usize) -> (f32, &'w [f32])) {
    let h = out.len();
    let mut at = 0;
    while at + 32 <= h {
        block::<32>(out, bias, at, n, &input);
        at += 32;
    }
    while at + 8 <= h {
        block::<8>(out, bias, at, n, &input);
        at += 8;
    }
    while at + 4 <= h {
        block::<4>(out, bias, at, n, &input);
        at += 4;
    }
    while at < h {
        block::<1>(out, bias, at, n, &input);
        at += 1;
    }
}

#[inline(always)]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    // four running sums, so the compiler can use vector instructions
    let mut s = [0.0f32; 4];
    let n = a.len().min(b.len());
    let (ca, cb) = (a[..n].chunks_exact(4), b[..n].chunks_exact(4));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for k in 0..4 {
            s[k] += x[k] * y[k];
        }
    }
    let mut t = s[0] + s[1] + s[2] + s[3];
    for (x, y) in ra.iter().zip(rb.iter()) {
        t += x * y;
    }
    t
}

/// What one pass over one view leaves behind: both hidden layers before and after the units'
/// curve, the score `g` and the number for each kind of turn.
#[derive(Clone)]
pub struct Pass {
    pub z1: [f32; MAX_H],
    pub a1: [f32; MAX_H],
    pub z2: [f32; MAX_H],
    pub a2: [f32; MAX_H],
    pub g: f32,
    pub logits: [f32; CLASSES],
}

impl Default for Pass {
    fn default() -> Pass {
        Pass { z1: [0.0; MAX_H], a1: [0.0; MAX_H], z2: [0.0; MAX_H], a2: [0.0; MAX_H], g: 0.0, logits: [0.0; CLASSES] }
    }
}

/// Where each part of the weights starts. Matrices are stored by input: for each input, the
/// weights to every unit of the next layer.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub t1: usize,
    pub b1: usize,
    pub t2: usize,
    pub b2: usize,
    pub vw: usize,
    pub vb: usize,
    pub tp: usize,
    pub pb: usize,
    pub len: usize,
}

pub fn layout(n_in: usize, h1: usize, h2: usize) -> Layout {
    let t1 = 0;
    let b1 = t1 + n_in * h1;
    let t2 = b1 + h1;
    let b2 = t2 + h1 * h2;
    let vw = b2 + h2;
    let vb = vw + h2;
    let tp = vb + 1;
    let pb = tp + h2 * CLASSES;
    Layout { t1, b1, t2, b2, vw, vb, tp, pb, len: pb + CLASSES }
}

#[derive(Clone, Debug)]
pub struct PvNet {
    pub map: String,
    /// inputs of one view this network reads: `IN`, or `IN_OLD` for one made before the
    /// opponent's third ring of walls
    pub n_in: usize,
    pub h1: usize,
    pub h2: usize,
    pub w: Vec<f32>,
}

/// The network's reading of a position.
pub struct Eval {
    /// fighter 0's result to come, from -1 (loses) to 1 (wins): 2 * chance to win - 1
    pub v0: f32,
    /// per fighter, the number for each kind of turn (higher: more likely the right turn)
    pub logits: [[f32; CLASSES]; 2],
}

impl PvNet {
    /// A network that knows nothing: every position even, every turn as likely as any other.
    pub fn blank(map: &str, h1: usize, h2: usize, seed: u64) -> PvNet {
        let l = layout(IN, h1, h2);
        let mut w = vec![0.0f32; l.len];
        let mut rng = seed | 1;
        let mut r = |scale: f32| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            ((rng % 20001) as f32 / 10000.0 - 1.0) * scale
        };
        for v in w[l.t1..l.b1].iter_mut() {
            *v = r(0.1);
        }
        for v in w[l.t2..l.b2].iter_mut() {
            *v = r(0.15);
        }
        // the two outputs start at zero
        PvNet { map: map.to_string(), n_in: IN, h1, h2, w }
    }

    /// Where each part of this network's weights starts.
    pub fn layout(&self) -> Layout {
        layout(self.n_in, self.h1, self.h2)
    }

    /// How many of a view's inputs this network reads: all, or the older ones only.
    #[inline]
    fn count(&self, x: &Inputs) -> usize {
        if self.n_in == IN { x.n } else { x.n0 }
    }

    /// This network reading every input there is now: one made with fewer gets the new ones
    /// with weights of zero, so it reads every position exactly as before until it is trained on.
    pub fn widened(&self) -> PvNet {
        if self.n_in == IN {
            return self.clone();
        }
        let old = self.layout();
        let mut w = Vec::with_capacity(layout(IN, self.h1, self.h2).len);
        w.extend_from_slice(&self.w[..old.b1]);
        w.resize(IN * self.h1, 0.0);
        w.extend_from_slice(&self.w[old.b1..]);
        PvNet { map: self.map.clone(), n_in: IN, h1: self.h1, h2: self.h2, w }
    }

    /// One pass over one view's inputs.
    pub fn forward(&self, x: &Inputs, p: &mut Pass) {
        self.hidden(x, p);
        self.numbers(p);
    }

    /// The first part of a pass: both hidden layers and the score `g`. A position's value needs
    /// only this much of each view.
    pub fn hidden(&self, x: &Inputs, p: &mut Pass) {
        let (h1, h2) = (self.h1, self.h2);
        let l = self.layout();
        let w = &self.w;
        layer(&mut p.z1[..h1], &w[l.b1..l.b1 + h1], self.count(x), |k| {
            let i = x.idx[k] as usize;
            (x.val[k], &w[l.t1 + i * h1..l.t1 + (i + 1) * h1])
        });
        for j in 0..h1 {
            p.a1[j] = act(p.z1[j]);
        }
        let a1 = &p.a1;
        layer(&mut p.z2[..h2], &w[l.b2..l.b2 + h2], h1, |i| (a1[i], &w[l.t2 + i * h2..l.t2 + (i + 1) * h2]));
        for j in 0..h2 {
            p.a2[j] = act(p.z2[j]);
        }
        p.g = w[l.vb] + dot(&w[l.vw..l.vw + h2], &p.a2[..h2]);
    }

    /// The rest of a pass: the number for each kind of turn, from the second hidden layer that
    /// `hidden` left in `p`.
    pub fn numbers(&self, p: &mut Pass) {
        let h2 = self.h2;
        let l = self.layout();
        let w = &self.w;
        let a2 = &p.a2;
        layer(&mut p.logits, &w[l.pb..l.pb + CLASSES], h2, |j| (a2[j], &w[l.tp + j * CLASSES..l.tp + (j + 1) * CLASSES]));
    }

    /// Adds to `grad` the slope of a loss with respect to every weight, given the loss's slope
    /// with respect to this view's score (`dg`) and to its number for each kind of turn.
    pub fn backward(&self, x: &Inputs, p: &Pass, dg: f32, dlogits: &[f32; CLASSES], grad: &mut [f32]) {
        let (h1, h2) = (self.h1, self.h2);
        let l = self.layout();
        let w = &self.w;
        grad[l.vb] += dg;
        axpy(&mut grad[l.pb..l.pb + CLASSES], 1.0, dlogits);
        let any_logit = dlogits.iter().any(|d| *d != 0.0);
        let mut dz2 = [0.0f32; MAX_H];
        for j in 0..h2 {
            grad[l.vw + j] += dg * p.a2[j];
            let mut da = dg * w[l.vw + j];
            if any_logit {
                axpy(&mut grad[l.tp + j * CLASSES..l.tp + (j + 1) * CLASSES], p.a2[j], dlogits);
                da += dot(&w[l.tp + j * CLASSES..l.tp + (j + 1) * CLASSES], dlogits);
            }
            dz2[j] = da * act_d(p.z2[j]);
        }
        axpy(&mut grad[l.b2..l.b2 + h2], 1.0, &dz2[..h2]);
        let mut dz1 = [0.0f32; MAX_H];
        for i in 0..h1 {
            axpy(&mut grad[l.t2 + i * h2..l.t2 + (i + 1) * h2], p.a1[i], &dz2[..h2]);
            dz1[i] = dot(&w[l.t2 + i * h2..l.t2 + (i + 1) * h2], &dz2[..h2]) * act_d(p.z1[i]);
        }
        axpy(&mut grad[l.b1..l.b1 + h1], 1.0, &dz1[..h1]);
        for k in 0..self.count(x) {
            let i = x.idx[k] as usize;
            axpy(&mut grad[l.t1 + i * h1..l.t1 + (i + 1) * h1], x.val[k], &dz1[..h1]);
        }
    }

    /// Reads a position: fighter 0's result to come and both fighters' turn numbers.
    pub fn eval(&self, e: &Engine, s: &State) -> Eval {
        let mut x = Inputs::default();
        let mut p = Pass::default();
        encode(e, s, 0, &mut x);
        self.forward(&x, &mut p);
        let (g0, l0) = (p.g, p.logits);
        encode(e, s, 1, &mut x);
        self.forward(&x, &mut p);
        Eval { v0: ((g0 - p.g) * 0.5).tanh(), logits: [l0, p.logits] }
    }

    pub fn save(&self, path: &std::path::Path, note: &serde_json::Value) -> std::io::Result<()> {
        let json = serde_json::json!({ "kind": "pvnet", "map": self.map, "h1": self.h1, "h2": self.h2, "in": self.n_in, "classes": CLASSES, "note": note, "w": self.w });
        std::fs::write(path, serde_json::to_string(&json).unwrap_or_default())
    }

    pub fn load(path: &std::path::Path) -> Option<PvNet> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let h1 = v.get("h1")?.as_u64()? as usize;
        let h2 = v.get("h2")?.as_u64()? as usize;
        let n_in = v.get("in")?.as_u64()? as usize;
        if (n_in != IN && n_in != IN_OLD) || v.get("classes")?.as_u64()? as usize != CLASSES || h1 > MAX_H || h2 > MAX_H {
            return None;
        }
        let w: Vec<f32> = v.get("w")?.as_array()?.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
        if w.len() != layout(n_in, h1, h2).len {
            return None;
        }
        Some(PvNet { map: v.get("map").and_then(|m| m.as_str()).unwrap_or("").to_string(), n_in, h1, h2, w })
    }
}

/// One searched position of a self-play game, for the trainer: the position itself, per fighter
/// the share of the search's visits on each kind of turn (out of 255), the search's own value
/// for fighter 0 (when the search counts in wins), and who won the game.
#[derive(Clone)]
pub struct Sample {
    pub s: State,
    pub pol: [[u8; CLASSES]; 2],
    pub q: f32,
    pub has_q: bool,
    /// 1 fighter 0 won, -1 fighter 1 won, 0 a draw
    pub z: i8,
    /// A position beside a game's path (`azdata::SideLog`): the game did not go through it, so
    /// there is no result and no turn shares to learn; its value target is `q` alone.
    pub side: bool,
}

/// Working room for `loss_grad`.
#[derive(Default)]
pub struct Scratch {
    pa: Pass,
    pb: Pass,
    xa: Inputs,
    xb: Inputs,
    turns: Vec<RawTurn>,
}

/// What one saved position costs under the network, for the trainer's reports.
#[derive(Default, Clone, Copy)]
pub struct Loss {
    /// how far the chance to win is from the target (cross-entropy)
    pub value: f32,
    /// the game had a winner, and whether the network's favourite was that winner
    pub decided: bool,
    pub value_hit: bool,
    /// the position was one beside a game's path (`Sample::side`)
    pub side: bool,
    /// summed over the fighters that had a choice: cross-entropy between the search's visit
    /// shares and the network's turn shares, the visit shares' own entropy, how many fighters,
    /// and for how many the network's first choice was the search's
    pub policy: f32,
    pub policy_floor: f32,
    pub policy_n: u32,
    pub top_hit: u32,
}

/// The value target for a saved position, as fighter 0's chance to win: the game's result,
/// mixed (`lambda`) with the search's own value where the search counted in wins. For a
/// position beside the game's path it is the search's value of that position alone.
pub fn value_target(r: &Sample, lambda: f32) -> f32 {
    let z = r.z as f32;
    let mix = if r.side { r.q } else if r.has_q { (1.0 - lambda) * z + lambda * r.q } else { z };
    0.5 * (1.0 + mix)
}

/// The loss of one saved position, and (with `grad`) its slope added for every weight:
/// cross-entropy of the chance to win against `value_target`, plus `pw` times the cross-entropy
/// of each fighter's turn shares against the search's visit shares.
pub fn loss_grad(net: &PvNet, e: &Engine, r: &Sample, lambda: f32, pw: f32, sc: &mut Scratch, grad: Option<&mut [f32]>) -> Loss {
    encode(e, &r.s, 0, &mut sc.xa);
    encode(e, &r.s, 1, &mut sc.xb);
    net.forward(&sc.xa, &mut sc.pa);
    net.forward(&sc.xb, &mut sc.pb);
    let d = sc.pa.g - sc.pb.g;
    let p = 1.0 / (1.0 + (-d).exp());
    let t = value_target(r, lambda);
    let eps = 1e-7f32;
    let mut out = Loss { value: -(t * (p + eps).ln() + (1.0 - t) * (1.0 - p + eps).ln()), decided: r.z != 0, value_hit: (d > 0.0) == (r.z > 0), side: r.side, ..Default::default() };
    let mut dl = [[0.0f32; CLASSES]; 2];
    for f in 0..2 {
        let logits = if f == 0 { &sc.pa.logits } else { &sc.pb.logits };
        let total: u32 = r.pol[f].iter().map(|v| *v as u32).sum();
        if total == 0 {
            continue;
        }
        // on offer: every turn the fighter may play here, and any kind the search visited
        let mut legal = [false; CLASSES];
        turns(e, &r.s, f, &mut sc.turns);
        for t in &sc.turns {
            legal[class_of(t)] = true;
        }
        for c in 0..CLASSES {
            legal[c] |= r.pol[f][c] > 0;
        }
        if legal.iter().filter(|l| **l).count() < 2 {
            continue;
        }
        let top = (0..CLASSES).filter(|c| legal[*c]).map(|c| logits[c]).fold(f32::MIN, f32::max);
        let mut sum = 0.0f32;
        let mut sm = [0.0f32; CLASSES];
        for c in 0..CLASSES {
            if legal[c] {
                sm[c] = (logits[c] - top).exp();
                sum += sm[c];
            }
        }
        let (mut best_net, mut best_pi) = ((f32::MIN, 0), (0u8, 0));
        for c in 0..CLASSES {
            if !legal[c] {
                continue;
            }
            let q = sm[c] / sum;
            let pi = r.pol[f][c] as f32 / total as f32;
            if pi > 0.0 {
                out.policy -= pi * (q + eps).ln();
                out.policy_floor -= pi * pi.ln();
            }
            dl[f][c] = pw * (q - pi);
            if logits[c] > best_net.0 {
                best_net = (logits[c], c);
            }
            if r.pol[f][c] > best_pi.0 {
                best_pi = (r.pol[f][c], c);
            }
        }
        out.policy_n += 1;
        out.top_hit += (best_net.1 == best_pi.1) as u32;
    }
    if let Some(grad) = grad {
        net.backward(&sc.xa, &sc.pa, p - t, &dl[0], grad);
        net.backward(&sc.xb, &sc.pb, t - p, &dl[1], grad);
    }
    out
}

/// The map's mirror images. A position mirrored left to right or top to bottom is the same
/// position when the walls and pads mirror onto themselves; the trainer shows the network every
/// saved position in each of these images, so what it learns in one corner it knows in all four.
pub struct Sym {
    w: i32,
    h: i32,
    /// which image: bit 0 mirrors x, bit 1 mirrors y; whether the map allows it
    pub ok: [bool; 4],
    /// for each image, the pad that lands where pad i stands
    pad: [Vec<usize>; 4],
    /// for each image, the direction each direction becomes
    dir: [[usize; 8]; 4],
}

impl Sym {
    pub fn of(e: &Engine) -> Sym {
        let (w, h) = (e.map.w, e.map.h);
        let flip = |k: usize, p: [i32; 2]| [if k & 1 != 0 { w - 1 - p[0] } else { p[0] }, if k & 2 != 0 { h - 1 - p[1] } else { p[1] }];
        let mut s = Sym { w, h, ok: [true; 4], pad: [Vec::new(), Vec::new(), Vec::new(), Vec::new()], dir: [[0; 8]; 4] };
        for k in 0..4 {
            for y in 0..h {
                for x in 0..w {
                    if e.map.is_wall([x, y]) != e.map.is_wall(flip(k, [x, y])) {
                        s.ok[k] = false;
                    }
                }
            }
            if flip(k, e.map.center) != e.map.center {
                s.ok[k] = false;
            }
            for (i, pd) in e.map.pads.iter().enumerate() {
                // the pad whose image stands on pad i
                match e.map.pads.iter().position(|q| flip(k, q.pos) == pd.pos && q.kind == pd.kind) {
                    Some(j) => s.pad[k].push(j),
                    None => {
                        s.pad[k].push(i);
                        s.ok[k] = false;
                    }
                }
            }
            for d in 0..8 {
                let v = DIR_VEC[d];
                let m = [if k & 1 != 0 { -v[0] } else { v[0] }, if k & 2 != 0 { -v[1] } else { v[1] }];
                s.dir[k][d] = DIR_VEC.iter().position(|u| *u == m).unwrap_or(d);
            }
        }
        s
    }

    /// The kind of turn that kind `c` becomes in image `k`.
    pub fn class(&self, k: usize, c: usize) -> usize {
        let (m, a) = (c / ATTACKS, c % ATTACKS);
        let m2 = match m {
            0 => 0,
            1..=8 => 1 + self.dir[k][m - 1],
            _ => 9 + self.dir[k][m - 9],
        };
        m2 * ATTACKS + a
    }

    /// The turn `t` as played in image `k`.
    pub fn turn(&self, k: usize, t: &RawTurn) -> RawTurn {
        let dir = |d: DirIn| match d {
            DirIn::Dir(d) => DirIn::Dir(self.dir[k][d as usize & 7] as u8),
            other => other,
        };
        let attack = match t.attack {
            AttackIn::Named { attack, target: crate::turn::TargetIn::Tile(p) } => AttackIn::Named { attack, target: crate::turn::TargetIn::Tile(self.tile(k, p)) },
            other => other,
        };
        RawTurn { mv: dir(t.mv), dash: dir(t.dash), attack, ..*t }
    }

    fn tile(&self, k: usize, p: [i32; 2]) -> [i32; 2] {
        [if k & 1 != 0 { self.w - 1 - p[0] } else { p[0] }, if k & 2 != 0 { self.h - 1 - p[1] } else { p[1] }]
    }

    /// The position `s` in image `k`: everything on the board mirrored, each pad's state moved
    /// to the pad its image stands on.
    pub fn state(&self, k: usize, s: &State) -> State {
        if k == 0 {
            return *s;
        }
        let sub = SUB as i32;
        let exact = |p: [i32; 2]| [if k & 1 != 0 { (self.w - 1) * sub - p[0] } else { p[0] }, if k & 2 != 0 { (self.h - 1) * sub - p[1] } else { p[1] }];
        let mut t = *s;
        for f in t.fighters.iter_mut() {
            f.pos = self.tile(k, f.pos);
        }
        for i in 0..t.n_projectiles as usize {
            let p = &mut t.projectiles[i];
            p.pos = self.tile(k, p.pos);
            p.origin = self.tile(k, p.origin);
            p.target = self.tile(k, p.target);
            p.exact_pos = exact(p.exact_pos);
        }
        for i in 0..t.n_areas as usize {
            t.areas[i].pos = self.tile(k, t.areas[i].pos);
            t.areas[i].origin = self.tile(k, t.areas[i].origin);
        }
        for (i, j) in self.pad[k].iter().enumerate() {
            t.pads[i] = s.pads[*j];
        }
        t
    }

    /// The saved position `r` in image `k`.
    pub fn image(&self, k: usize, r: &Sample) -> Sample {
        let mut out = Sample { s: self.state(k, &r.s), pol: [[0; CLASSES]; 2], q: r.q, has_q: r.has_q, z: r.z, side: r.side };
        for f in 0..2 {
            for c in 0..CLASSES {
                out.pol[f][self.class(k, c)] = r.pol[f][c];
            }
        }
        out
    }
}
