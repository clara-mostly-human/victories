//! Goal inference for an opponent model (part 3, next version): instead of guessing a bot's next
//! step, infer where it is trying to be. A bot's single steps are noisy, but once it has made two
//! or three moves toward a pad, the centre or us, only a few paths to that goal remain.
//!
//! Goals: each pad, the centre, the other fighter's tile (closing in), and "away" (the tile of
//! the map farthest from the other fighter that it can reach soonest). For each goal, a recent
//! step that shortens the path to it is likely (and one that keeps the distance is less so);
//! the goals' chances follow from the last few steps. The forecast walks the most likely goal's
//! shortest path at one step every `move_cooldown` ticks.

use crate::geom::{add, chebyshev, DIR_VEC, P};
use crate::state::State;
use crate::Engine;
use std::collections::VecDeque;

/// Steps from every tile to `target` (u16::MAX where it cannot be reached).
pub fn dist_map(e: &Engine, target: P) -> Vec<u16> {
    let w = e.map.w;
    let mut d = vec![u16::MAX; (w * e.map.h) as usize];
    if !e.map.in_bounds(target) || e.map.is_wall(target) {
        return d;
    }
    let idx = |p: P| (p[1] * w + p[0]) as usize;
    let mut q = VecDeque::new();
    d[idx(target)] = 0;
    q.push_back(target);
    while let Some(p) = q.pop_front() {
        let dp = d[idx(p)];
        for dir in 0..8u8 {
            let from = add(p, DIR_VEC[((dir + 4) % 8) as usize]);
            if !e.map.in_bounds(from) || e.map.is_wall(from) || e.map.step_blocked(from, dir) {
                continue;
            }
            if d[idx(from)] == u16::MAX {
                d[idx(from)] = dp + 1;
                q.push_back(from);
            }
        }
    }
    d
}

/// The goals for fighter `x` at state `s`: (tile, its distance map).
pub fn goals(e: &Engine, s: &State, x: usize) -> Vec<(P, Vec<u16>)> {
    let o = &s.fighters[1 - x];
    let mut out: Vec<(P, Vec<u16>)> = e.map.pads.iter().map(|p| (p.pos, dist_map(e, p.pos))).collect();
    out.push((e.map.center, dist_map(e, e.map.center)));
    out.push((o.pos, dist_map(e, o.pos)));
    // away: the reachable tile farthest from the other fighter (ties: nearer to x)
    let from_o = dist_map(e, o.pos);
    let from_x = dist_map(e, s.fighters[x].pos);
    let w = e.map.w;
    let mut best: Option<(i32, P)> = None;
    for y in 0..e.map.h {
        for xx in 0..w {
            let i = (y * w + xx) as usize;
            if from_o[i] == u16::MAX || from_x[i] == u16::MAX || from_x[i] > 8 {
                continue;
            }
            let score = 3 * from_o[i] as i32 - from_x[i] as i32;
            if best.map(|b| score > b.0).unwrap_or(true) {
                best = Some((score, [xx, y]));
            }
        }
    }
    if let Some((_, p)) = best {
        out.push((p, dist_map(e, p)));
    }
    out
}

/// The chance of each goal for fighter `x`, from its tiles over the last ticks (`trail`, oldest
/// first, ending at its tile now). Uniform when it has not moved.
pub fn infer(e: &Engine, goals: &[(P, Vec<u16>)], trail: &[P]) -> Vec<f64> {
    let w = e.map.w;
    let at = |g: usize, p: P| goals[g].1.get((p[1] * w + p[0]) as usize).cloned().unwrap_or(u16::MAX) as i32;
    let mut logp = vec![0.0f64; goals.len()];
    for win in trail.windows(2) {
        let (a, b) = (win[0], win[1]);
        if a == b {
            continue;
        }
        for g in 0..goals.len() {
            let (da, db) = (at(g, a), at(g, b));
            logp[g] += if db < da { (0.8f64).ln() } else if db == da { (0.25f64).ln() } else { (0.05f64).ln() };
        }
    }
    let m = logp.iter().cloned().fold(f64::MIN, f64::max);
    let mut p: Vec<f64> = logp.iter().map(|l| (l - m).exp()).collect();
    let sum: f64 = p.iter().sum();
    for v in p.iter_mut() {
        *v /= sum.max(1e-12);
    }
    p
}

/// Where fighter `x` will stand `ticks` from now if it walks goal `g`'s shortest path, a step every
/// `cooldown` ticks, stopping at the goal.
pub fn walk(e: &Engine, goal: &[u16], from: P, ticks: i32, cooldown: i32) -> P {
    let w = e.map.w;
    let at = |p: P| goal.get((p[1] * w + p[0]) as usize).cloned().unwrap_or(u16::MAX);
    let mut pos = from;
    for _ in 0..(ticks / cooldown.max(1)) {
        let mut best = (at(pos), None);
        for dir in 0..8u8 {
            let q = add(pos, DIR_VEC[dir as usize]);
            if e.map.step_blocked(pos, dir) || !e.map.in_bounds(q) {
                continue;
            }
            if at(q) < best.0 {
                best = (at(q), Some(q));
            }
        }
        match best.1 {
            Some(q) => pos = q,
            None => break,
        }
    }
    pos
}

/// The forecast of fighter `x`'s tile `ticks` from now: the most likely goal's path, or staying
/// put when no goal stands out (the best chance under `min_share`).
pub fn forecast(e: &Engine, s: &State, x: usize, trail: &[P], ticks: i32, min_share: f64) -> P {
    let gs = goals(e, s, x);
    let p = infer(e, &gs, trail);
    let here = s.fighters[x].pos;
    let Some((g, share)) = p.iter().cloned().enumerate().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()) else { return here };
    if share < min_share {
        return here;
    }
    walk(e, &gs[g].1, here, ticks, e.rules.move_cooldown.max(1))
}

/// Distance between two tiles, for reports.
pub fn miss(a: P, b: P) -> i32 {
    chebyshev(a, b)
}
