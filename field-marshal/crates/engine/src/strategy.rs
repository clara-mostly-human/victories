//! The strategy layer (Mike's plan): three phases, each with goals that depend on whether a side
//! is ahead, even or behind in hp and power, choosing a mode with concrete targets. The search
//! plays to the mode through a small goal score at its leaves; tactics (shots, slams, dodges)
//! stay with the search and the judgment in every phase.
//!
//! Opening (until both first power pads are taken): get the first power-up; with it, deal damage
//! and take or deny the second; without it, make sure of the other one.
//! Middle: ahead in hp, power or both, press (stay in range, keep sight, stand between them and
//! their heal); even, engage; behind, disengage (break sight, open distance, heal, wait out their
//! buff).
//! End (from tick 400): ahead or even, engage until one dies; behind, kite (keep distance) and take
//! a health pad if one is there.

use crate::geom::{chebyshev, P};
use crate::map::PadKind;
use crate::observe::can_see;
use crate::state::{State, NO_BUFF};
use crate::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// opening: go to this power pad (the first, the second to take or deny, or the other one)
    Pad(P),
    Press,
    Engage,
    Disengage,
    Kite,
}

pub const MODE_NAMES: [&str; 5] = ["power pad", "press", "engage", "disengage", "kite"];

pub fn mode_index(m: Mode) -> usize {
    match m {
        Mode::Pad(_) => 0,
        Mode::Press => 1,
        Mode::Engage => 2,
        Mode::Disengage => 3,
        Mode::Kite => 4,
    }
}

/// 0 opening, 1 middle, 2 end.
pub fn phase(e: &Engine, s: &State) -> u8 {
    if s.tick >= 400.min(e.rules.max_ticks * 2 / 3) {
        return 2;
    }
    let first = e.rules.pickups.as_ref().map(|p| p.first_spawn).unwrap_or(20);
    let untaken = e.map.pads.iter().enumerate().any(|(i, d)| d.kind == PadKind::Power && s.pads[i].available);
    if s.tick < first || (untaken && s.tick < 120) {
        0
    } else {
        1
    }
}

fn buff_left(s: &State, x: usize) -> i32 {
    let f = &s.fighters[x];
    if f.power_until == NO_BUFF { 0 } else { (f.power_until - s.tick).max(0) }
}

/// Fighter `x`'s edge in hp: its hp lead plus half a hp for every tick of power buff it has more.
pub fn edge(s: &State, x: usize) -> f64 {
    (s.fighters[x].hp - s.fighters[1 - x].hp) as f64 + 0.5 * (buff_left(s, x) - buff_left(s, 1 - x)) as f64
}

thread_local! {
    static MAPS: std::cell::RefCell<std::collections::HashMap<(String, P), Vec<u16>>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Path length from `from` to `to` (cached distance maps per map and target).
pub fn path(e: &Engine, to: P, from: P) -> i32 {
    MAPS.with(|m| {
        let mut m = m.borrow_mut();
        let d = m.entry((e.map.name.clone(), to)).or_insert_with(|| crate::goalmodel::dist_map(e, to));
        d.get((from[1] * e.map.w + from[0]) as usize).map(|v| if *v == u16::MAX { 99 } else { *v as i32 }).unwrap_or(99)
    })
}

/// The mode fighter `x` plays in this position.
pub fn choose(e: &Engine, s: &State, x: usize) -> Mode {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let ed = edge(s, x);
    match phase(e, s) {
        0 => {
            let pads: Vec<(usize, P)> = e.map.pads.iter().enumerate().filter(|(_, d)| d.kind == PadKind::Power).map(|(i, d)| (i, d.pos)).collect();
            let open: Vec<P> = pads.iter().filter(|(i, _)| s.pads[*i].available || s.tick < 20).map(|(_, p)| *p).collect();
            if open.is_empty() {
                return Mode::Engage;
            }
            // only a pad we can still contest: up, or back by the time we get there, and we are no
            // farther from it than the opponent plus a step; otherwise engage and stay level
            let first = e.rules.pickups.as_ref().map(|p| p.first_spawn).unwrap_or(20);
            let pace = e.rules.move_cooldown.max(1);
            let contest: Vec<P> = open
                .iter()
                .filter(|p| path(e, **p, f.pos) <= path(e, **p, o.pos) + 1 && (s.tick >= first || path(e, **p, f.pos) * pace + s.tick >= first - 4))
                .cloned()
                .collect();
            match contest.iter().min_by_key(|p| path(e, **p, f.pos)) {
                Some(p) => Mode::Pad(*p),
                None => Mode::Engage,
            }
        }
        1 => {
            if ed >= 15.0 {
                Mode::Press
            } else if ed <= -15.0 {
                Mode::Disengage
            } else {
                Mode::Engage
            }
        }
        _ => {
            if ed <= -15.0 {
                Mode::Kite
            } else {
                Mode::Engage
            }
        }
    }
}

/// The nearest health pad that is up or back within `within` ticks.
fn health_pad(e: &Engine, s: &State, from: P, within: i32) -> Option<P> {
    e.map
        .pads
        .iter()
        .enumerate()
        .filter(|(i, d)| d.kind == PadKind::Health && (s.pads[*i].available || s.pads[*i].respawn_tick - s.tick <= within))
        .map(|(_, d)| d.pos)
        .min_by_key(|p| path(e, *p, from))
}

/// How well fighter `x`'s position serves mode `m`, in hp, capped at +-`cap`.
pub fn score(e: &Engine, s: &State, x: usize, m: Mode, cap: f64) -> f64 {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if !f.alive || !o.alive {
        return 0.0;
    }
    let d = chebyshev(f.pos, o.pos) as f64;
    let sight = can_see(&e.rules, &e.map, f.pos, o.pos);
    let v = match m {
        Mode::Pad(p) => {
            if buff_left(s, x) > 0 && f.pos == p {
                cap
            } else {
                -0.25 * path(e, p, f.pos) as f64
            }
        }
        Mode::Press => {
            // in range and in sight, between them and their heal
            // keep them from getting away (past 5 tiles) and in sight; how close to fight is left to
            // the search's tactics (pulling to 3 tiles cost 13 to 14 hp a game in pressing play)
            // keep them from getting away (past 5 tiles) and in sight
            let mut v = -0.4 * (d - 5.0).max(0.0) + if sight { 1.0 } else { -1.0 };
            // deny the heal: graded by how much nearer their heal we stand than they do, once they
            // are hurt enough to want it
            if o.hp < e.rules.max_hp - 15 {
                if let Some(h) = health_pad(e, s, o.pos, 40) {
                    v += 0.4 * ((path(e, h, o.pos) - path(e, h, f.pos)) as f64).clamp(-5.0, 5.0);
                }
            }
            v
        }
        Mode::Engage => -0.5 * (d - 3.0).abs() + if sight { 1.0 } else { 0.0 },
        Mode::Disengage => {
            // distance and broken sight count once out of their reach; within it every tile counts against
            let reach = 4.0;
            let mut v = if d <= reach { -0.8 * (reach + 1.0 - d) } else { 0.4 * d.min(8.0) + if sight { -1.0 } else { 1.5 } };
            // get the heal: nearer it than they are, and as near as we can
            if f.hp < e.rules.max_hp - 15 {
                if let Some(h) = health_pad(e, s, f.pos, 40) {
                    v -= 0.3 * path(e, h, f.pos).min(15) as f64;
                    v += 0.3 * ((path(e, h, o.pos) - path(e, h, f.pos)) as f64).clamp(-5.0, 5.0);
                }
            }
            v
        }
        Mode::Kite => {
            let band = if d < 5.0 { 5.0 - d } else if d > 7.0 { d - 7.0 } else { 0.0 };
            let mut v = -0.6 * band;
            if f.hp < e.rules.max_hp - 20 {
                if let Some(h) = health_pad(e, s, f.pos, 20) {
                    v -= 0.3 * path(e, h, f.pos).min(15) as f64;
                }
            }
            v
        }
    };
    v.clamp(-cap, cap)
}
