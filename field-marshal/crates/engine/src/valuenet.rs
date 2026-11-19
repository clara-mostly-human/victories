//! A learned judgment of positions as a win probability: a small network over the whole
//! position, trained on replays of strong bots (`winfit` in the conformance crate) to say
//! who goes on to win.
//!
//! Inputs: for each fighter its `valuefeat::phi` vector (all features) and a few more (how
//! far it stands from the centre, walls next to it), and for the position its context (tick,
//! zone radius, distance between the fighters, whether they see each other, which map).
//! The network `g` reads (mine, theirs, context); the judgment for fighter `x` is
//! `sigmoid(g(x's, other's, ctx) - g(other's, x's, ctx))`, so the two fighters' chances add
//! up to one by construction.

use crate::geom::{add, chebyshev, DIR_VEC};
use crate::observe::can_see;
use crate::state::State;
use crate::valuefeat::{phi, NV};
use crate::Engine;

/// Inputs for one fighter: `phi` plus ten (centre distances, walls next to it, and the
/// tactics in motion: shots flying at it and how soon the nearest lands, a frostburst about
/// to go off on it and how soon, its stun or other non-idle ticks left, its move cooldown).
pub const OWN: usize = NV + 10;
/// Inputs for the position.
pub const CTX: usize = 7;
/// Inputs of `g`.
pub const IN: usize = 2 * OWN + CTX;
pub const H1: usize = 32;
pub const H2: usize = 16;
/// Number of weights: two dense layers with biases and the output with its bias.
pub const NW: usize = IN * H1 + H1 + H1 * H2 + H2 + H2 + 1;

/// One fighter's inputs.
pub fn own(e: &Engine, s: &State, x: usize) -> [f32; OWN] {
    let mut v = [0.0f32; OWN];
    v[..NV].copy_from_slice(&phi(e, s, x, true));
    let f = &s.fighters[x];
    let c = e.map.center;
    v[NV] = (f.pos[0] - c[0]).abs() as f32 / 10.0;
    v[NV + 1] = (f.pos[1] - c[1]).abs() as f32 / 10.0;
    v[NV + 2] = chebyshev(f.pos, c) as f32 / 10.0;
    let walls = (0..8u8).filter(|d| e.map.step_blocked(f.pos, *d) || e.map.is_wall(add(f.pos, DIR_VEC[*d as usize]))).count();
    v[NV + 3] = walls as f32 / 8.0;
    // shots of the other fighter heading at this one (within 8 tiles), and the soonest
    let mut shots = 0.0f32;
    let mut soonest = 8i32;
    for p in s.projectiles().iter().filter(|p| p.owner as usize != x) {
        let d = chebyshev(p.pos, f.pos);
        if d <= 8 && chebyshev(p.target, f.pos) <= 1 {
            shots += 1.0;
            soonest = soonest.min((d + p.speed.max(1) - 1) / p.speed.max(1));
        }
    }
    v[NV + 4] = shots.min(3.0) / 3.0;
    v[NV + 5] = if shots > 0.0 { 1.0 - soonest as f32 / 8.0 } else { 0.0 };
    // a frostburst of the other fighter that will catch this one, and how soon
    let mut burst_soon = 0.0f32;
    for a in s.areas().iter().filter(|a| a.owner as usize != x) {
        if e.map.in_blast(a.pos, a.radius, f.pos) {
            burst_soon = burst_soon.max(1.0 - ((a.explodes_tick - s.tick).clamp(0, 8) as f32) / 8.0);
        }
    }
    v[NV + 6] = (burst_soon > 0.0) as i32 as f32;
    v[NV + 7] = burst_soon;
    v[NV + 8] = if f.state == crate::state::FState::Idle { 0.0 } else { ((f.state_end - s.tick).clamp(0, 8)) as f32 / 8.0 };
    v[NV + 9] = f.cd_move.clamp(0, 4) as f32 / 4.0;
    v
}

/// The position's context.
pub fn ctx(e: &Engine, s: &State) -> [f32; CTX] {
    let (a, b) = (&s.fighters[0], &s.fighters[1]);
    let map = crate::shipped::MAP_NAMES.iter().position(|n| *n == e.map.name).unwrap_or(0);
    [
        s.tick as f32 / e.rules.max_ticks.max(1) as f32,
        s.zone_radius.clamp(0, 15) as f32 / 10.0,
        chebyshev(a.pos, b.pos) as f32 / 20.0,
        can_see(&e.rules, &e.map, a.pos, b.pos) as i32 as f32,
        (map == 0) as i32 as f32,
        (map == 1) as i32 as f32,
        (map == 2) as i32 as f32,
    ]
}

/// The network's raw output for (mine, theirs, context).
pub fn g(w: &[f32], mine: &[f32; OWN], theirs: &[f32; OWN], c: &[f32; CTX]) -> f32 {
    let mut input = [0.0f32; IN];
    input[..OWN].copy_from_slice(mine);
    input[OWN..2 * OWN].copy_from_slice(theirs);
    input[2 * OWN..].copy_from_slice(c);
    let (w1, rest) = w.split_at(IN * H1);
    let (b1, rest) = rest.split_at(H1);
    let (w2, rest) = rest.split_at(H1 * H2);
    let (b2, rest) = rest.split_at(H2);
    let (w3, b3) = rest.split_at(H2);
    let mut h1 = [0.0f32; H1];
    for j in 0..H1 {
        let row = &w1[j * IN..(j + 1) * IN];
        let mut z = b1[j];
        for i in 0..IN {
            z += row[i] * input[i];
        }
        h1[j] = z.max(0.0);
    }
    let mut h2 = [0.0f32; H2];
    for j in 0..H2 {
        let row = &w2[j * H1..(j + 1) * H1];
        let mut z = b2[j];
        for i in 0..H1 {
            z += row[i] * h1[i];
        }
        h2[j] = z.max(0.0);
    }
    let mut out = b3[0];
    for j in 0..H2 {
        out += w3[j] * h2[j];
    }
    out
}

/// The network's log-odds of fighter `x` winning.
pub fn logit(e: &Engine, s: &State, x: usize, w: &[f32]) -> f32 {
    if w.len() != NW {
        return 0.0;
    }
    let (a, b, c) = (own(e, s, x), own(e, s, 1 - x), ctx(e, s));
    g(w, &a, &b, &c) - g(w, &b, &a, &c)
}

/// The network's verdict for fighter `x` read as an hp lead: the lead that alone would give
/// the same chance of winning (`hp_per_logit` from the trainer's hp-alone fit).
pub fn hp_equiv(e: &Engine, s: &State, x: usize, w: &[f32], hp_per_logit: f32) -> f32 {
    logit(e, s, x, w) * hp_per_logit
}

/// The hp fighter `x` is expected to gain or lose from here (discounted 0.98 a tick, 40 for a
/// kill), by weights trained with `winlab --target hp` (the network's output is in units of 32 hp).
pub fn hp_to_come(e: &Engine, s: &State, x: usize, w: &[f32]) -> f32 {
    32.0 * logit(e, s, x, w)
}

/// Fighter `x`'s chance of winning from this position, by the weights given.
pub fn win_prob(e: &Engine, s: &State, x: usize, w: &[f32]) -> f32 {
    if w.len() != NW {
        return 0.5;
    }
    let (a, b, c) = (own(e, s, x), own(e, s, 1 - x), ctx(e, s));
    let z = g(w, &a, &b, &c) - g(w, &b, &a, &c);
    1.0 / (1.0 + (-z).exp())
}
