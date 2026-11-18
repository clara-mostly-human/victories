//! A sparring partner cloned from the ladder's strong bots: each tick it samples a move
//! and an attack from the fitted models in `models`. It does no search and no dodging of
//! its own, so it is weaker than the bots it imitates; its use is as a fast local judge
//! whose verdicts are first checked against changes the ladder has already judged.
//!
//! It is given the true state (it never has to track us in the fog), which stands in for
//! the good tracking the real bots have.

use crate::geom::{add, chebyshev, DIR_VEC};
use crate::models::{move_probs, yes_prob, BURST_WEIGHTS, FROST_WEIGHTS, SHOT_WEIGHTS, SPENT_WEIGHTS};
use crate::movefeat::features;
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State};
use crate::turn::RawTurn;
use crate::Engine;

/// A small deterministic generator (xorshift), so games can be replayed from a seed.
pub fn next(rng: &mut u64) -> f32 {
    let mut x = *rng;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *rng = x;
    ((x >> 40) as f32) / ((1u64 << 24) as f32)
}

/// The clone's turn for fighter `x`.
pub fn clone_turn(e: &Engine, s: &State, x: usize, rng: &mut u64) -> RawTurn {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if f.state != FState::Idle || !o.alive {
        return RawTurn::IDLE;
    }
    let d = chebyshev(f.pos, o.pos);
    // A step of the wanted kind: 1 toward, 2 away, 3 sideways. Random among those that fit.
    let step_of = |kind: usize, rng: &mut u64| -> Option<u8> {
        let mut fit: Vec<u8> = Vec::new();
        for dir in 0..8u8 {
            if e.map.step_blocked(f.pos, dir) {
                continue;
            }
            let dest = add(f.pos, DIR_VEC[dir as usize]);
            if dest == o.pos {
                continue;
            }
            let after = chebyshev(dest, o.pos);
            let k = if after < d { 1 } else if after > d { 2 } else { 3 };
            if k == kind {
                fit.push(dir);
            }
        }
        if fit.is_empty() {
            None
        } else {
            Some(fit[((next(rng) * fit.len() as f32) as usize).min(fit.len() - 1)])
        }
    };
    if !can_see(&e.rules, &e.map, f.pos, o.pos) {
        // Out of sight: close in.
        return match (f.cd_move, step_of(1, rng)) {
            (0, Some(dir)) => RawTurn::step(dir),
            _ => RawTurn::IDLE,
        };
    }
    let feat = features(e, s, x);
    // The dash direction: away when standing in a burst or facing a ready shot with ours
    // spent, otherwise in. Each direction is tried on the engine to see where it ends.
    let dash_dir = |rng: &mut u64| -> Option<u8> {
        let flee = feat[30] > 0.5 || (feat[11] > 0.5 && feat[8] < 0.5);
        let mut best: Option<(i32, u8)> = None;
        for dir in 0..8u8 {
            let mut after = *s;
            let turns = if x == 0 { [RawTurn::dash(dir), RawTurn::IDLE] } else { [RawTurn::IDLE, RawTurn::dash(dir)] };
            e.step(&mut after, &turns);
            let end = after.fighters[x].pos;
            if end == f.pos {
                continue;
            }
            let dist = chebyshev(end, o.pos);
            let score = if flee { dist } else { -(dist - 2).abs() } * 16 + (next(rng) * 8.0) as i32;
            if best.map(|(b, _)| score > b).unwrap_or(true) {
                best = Some((score, dir));
            }
        }
        best.map(|(_, dir)| dir)
    };
    let mut turn = RawTurn::IDLE;
    if f.cd_move == 0 {
        let p = move_probs(&feat);
        let roll = next(rng);
        let mut acc = 0.0;
        let mut class = 0;
        for (c, pc) in p.iter().enumerate() {
            acc += pc;
            if roll < acc {
                class = c;
                break;
            }
        }
        match class {
            1..=3 => {
                if let Some(dir) = step_of(class, rng) {
                    turn = RawTurn::step(dir);
                }
            }
            4 if f.cd_dash == 0 => {
                if let Some(dir) = dash_dir(rng) {
                    turn = RawTurn::dash(dir);
                }
            }
            _ => {}
        }
    } else if f.cd_dash == 0 && next(rng) < yes_prob(&SPENT_WEIGHTS, &feat) {
        if let Some(dir) = dash_dir(rng) {
            turn = RawTurn::dash(dir);
        }
    }
    // One attack at most: the shot first, then the repulsor, then the frostburst.
    let ready = |want: u8| {
        e.rules.attacks.iter().enumerate().find(|(i, a)| {
            f.cd_attack[*i] == 0
                && match (&a.kind, want) {
                    (AttackKind::Projectile { .. }, 0) => true,
                    (AttackKind::Area { range, .. }, 1) => d <= *range,
                    (AttackKind::Burst { .. }, 2) => d <= 2,
                    _ => false,
                }
        }).map(|(i, _)| i as u8)
    };
    if let Some(a) = ready(0).filter(|_| next(rng) < yes_prob(&SHOT_WEIGHTS, &feat)) {
        turn = turn.with_attack(a, o.pos);
    } else if let Some(a) = ready(2).filter(|_| next(rng) < yes_prob(&BURST_WEIGHTS, &feat)) {
        turn = turn.with_burst(a);
    } else if let Some(a) = ready(1).filter(|_| next(rng) < yes_prob(&FROST_WEIGHTS, &feat)) {
        turn = turn.with_attack(a, o.pos);
    }
    turn
}
