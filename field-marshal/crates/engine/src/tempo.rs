//! Tempo features: who holds the initiative in a state, in numbers a search can weigh.
//!
//! The fight is decided at close range by cooldowns: a fighter that cannot step is a free
//! hit for an opponent whose shot is ready, and a free slam if a wall is behind it. These
//! features name those situations. Their weights are fitted from ladder replays (see
//! `conformance fit-eval`): each weight is the hp swing that followed, on average, over
//! the next ticks of real games, most of them between the ladder's best bots.

use crate::geom::{add, chebyshev, octant, DIR_VEC, P};
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::Engine;

pub const N: usize = 19;

pub const NAMES: [&str; N] = [
    "can step",
    "step two or more ticks away",
    "cannot act (stunned or recovering), ticks",
    "shot ready",
    "shot cooldown fraction",
    "dash ready",
    "repulsor ready",
    "frostburst ready",
    "power buff",
    "outside zone",
    "hp fraction",
    "stuck, their shot ready, within 2, in sight",
    "stuck, their shot and step ready, at 3, in sight",
    "stuck, their shot and dash ready, at 3 to 5",
    "stuck with no dash, their shot ready, within 3",
    "can step, their shot ready, within 2",
    "their repulsor ready, within 2, wall behind us",
    "their repulsor and dash ready, at 3 to 4, wall behind us",
    "stuck, their repulsor ready, within 2",
];

fn weapon_ready(e: &Engine, s: &State, x: usize, want: fn(&AttackKind) -> bool) -> (bool, f32) {
    for (i, a) in e.rules.attacks.iter().enumerate() {
        if want(&a.kind) {
            let cd = s.fighters[x].cd_attack[i];
            return (cd == 0, cd as f32 / a.cooldown.max(1) as f32);
        }
    }
    (false, 1.0)
}

/// Would a shove from `from` push a fighter standing on `p` into a wall within `tiles`?
fn wall_behind(e: &Engine, from: P, p: P, tiles: i32) -> bool {
    let Some(dir) = octant(from, p) else { return false };
    let mut q = p;
    for _ in 0..tiles {
        if e.map.step_blocked(q, dir) {
            return true;
        }
        q = add(q, DIR_VEC[dir as usize]);
    }
    false
}

/// The features of fighter `x`'s situation against the other fighter.
pub fn phi(e: &Engine, s: &State, x: usize) -> [f32; N] {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let d = chebyshev(f.pos, o.pos);
    let sight = can_see(&e.rules, &e.map, o.pos, f.pos);
    let can_step = f.state == FState::Idle && f.cd_move == 0;
    let o_can_step = o.state == FState::Idle && o.cd_move == 0;
    let stuck = !can_step;
    let (shot, shot_cd) = weapon_ready(e, s, x, |k| matches!(k, AttackKind::Projectile { .. }));
    let (burst, _) = weapon_ready(e, s, x, |k| matches!(k, AttackKind::Burst { .. }));
    let (frost, _) = weapon_ready(e, s, x, |k| matches!(k, AttackKind::Area { .. }));
    let (o_shot, _) = weapon_ready(e, s, 1 - x, |k| matches!(k, AttackKind::Projectile { .. }));
    let (o_burst, _) = weapon_ready(e, s, 1 - x, |k| matches!(k, AttackKind::Burst { .. }));
    let o_acts = o.state == FState::Idle;
    let dash = f.state == FState::Idle && f.cd_dash == 0;
    let o_dash = o_acts && o.cd_dash == 0;
    let push = match e.rules.attacks.iter().find_map(|a| if let AttackKind::Burst { push } = a.kind { Some(push) } else { None }) {
        Some(p) => p,
        None => 0,
    };
    let b = |x: bool| x as i32 as f32;
    let wall = wall_behind(e, o.pos, f.pos, push);
    let waiting = if f.state == FState::Stunned || f.state == FState::Recovering { (f.state_end - s.tick + 1).max(0) as f32 } else { 0.0 };
    [
        b(can_step),
        b(f.cd_move >= 2),
        waiting,
        b(shot && f.state == FState::Idle),
        shot_cd,
        b(dash),
        b(burst && f.state == FState::Idle),
        b(frost && f.state == FState::Idle),
        b(f.power_until != NO_BUFF),
        b(chebyshev(f.pos, e.map.center) > s.zone_radius),
        f.hp as f32 / e.rules.max_hp.max(1) as f32,
        b(stuck && o_shot && o_acts && d <= 2 && sight),
        b(stuck && o_shot && o_can_step && d == 3 && sight),
        b(stuck && o_shot && o_dash && (3..=5).contains(&d)),
        b(stuck && !dash && o_shot && o_acts && d <= 3),
        b(can_step && o_shot && o_acts && d <= 2),
        b(o_burst && o_acts && d <= 2 && wall),
        b(o_burst && o_dash && (3..=4).contains(&d) && wall),
        b(stuck && o_burst && o_acts && d <= 2),
    ]
}

/// Our features minus the opponent's: positive entries favour fighter `me`.
pub fn diff(e: &Engine, s: &State, me: usize) -> [f32; N] {
    let a = phi(e, s, me);
    let b = phi(e, s, 1 - me);
    let mut out = [0.0; N];
    for i in 0..N {
        out[i] = a[i] - b[i];
    }
    out
}

/// Fitted by `conformance fit-eval ../replays/corpus --horizon 12` on 1,287 ladder replays
/// (222,521 ticks, 2026-11-18): hp swing over the next 12 ticks per unit of each feature.
pub const WEIGHTS: [f32; N] = [
    -0.012, -2.636, -0.102, -0.243, -2.282, 1.947, -0.007, 0.846, 4.648, -9.616, -1.111, -5.053, -0.941, -0.710, -1.715, -3.333,
    -3.626, -1.309, -3.751,
];

/// Expected hp swing in our favour over the next ticks, from fitted weights.
#[inline]
pub fn value(e: &Engine, s: &State, me: usize, w: &[f32; N]) -> f32 {
    let d = diff(e, s, me);
    let mut v = 0.0;
    for i in 0..N {
        v += w[i] * d[i];
    }
    v
}
