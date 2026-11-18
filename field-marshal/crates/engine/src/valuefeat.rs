//! Features for a judgment of positions: what a position is worth beyond what the search
//! can see. One vector per fighter; the judgment is a weighted sum of the difference
//! between the two fighters' vectors, so it is zero for mirrored positions and its sign
//! flips with the point of view. The weights are fitted on the search's own games
//! (`valuefit` in the conformance crate) to the hp it went on to gain or lose.

use crate::geom::{add, chebyshev, octant, DIR_VEC, P};
use crate::map::PadKind;
use crate::observe::next_shrink;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::Engine;

pub const NV: usize = 31;
/// The first features, which the first generations of the judgment were fitted on; the
/// rest (`EXTENDED`) cost more to compute and are skipped when their weights are all zero.
pub const BASIC: usize = 23;

fn wall_behind(e: &Engine, from: P, threat: P, reach: i32) -> bool {
    let Some(dir) = octant(threat, from) else { return false };
    let mut p = from;
    for _ in 0..reach {
        if e.map.step_blocked(p, dir) {
            return true;
        }
        p = add(p, DIR_VEC[dir as usize]);
    }
    false
}

/// The vector of fighter `x`; with `extended` off the features past `BASIC` are left at zero.
pub fn phi(e: &Engine, s: &State, x: usize, extended: bool) -> [f32; NV] {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let b = |v: bool| v as i32 as f32;
    let cd = |pred: &dyn Fn(&AttackKind) -> bool| -> i32 {
        e.rules.attacks.iter().enumerate().filter(|(_, a)| pred(&a.kind)).map(|(i, _)| f.cd_attack[i]).min().unwrap_or(99)
    };
    let shot = cd(&|k| matches!(k, AttackKind::Projectile { .. }));
    let frost = cd(&|k| matches!(k, AttackKind::Area { .. }));
    let burst = cd(&|k| matches!(k, AttackKind::Burst { .. }));
    let d = chebyshev(f.pos, o.pos);
    let max = e.rules.max_hp as f32;
    let hp = f.hp.max(0) as f32 / max;
    let buff = if f.power_until == NO_BUFF { 0 } else { (f.power_until - s.tick).max(0) };
    let from_centre = chebyshev(f.pos, e.map.center);
    let outside_next = match next_shrink(&e.rules, &e.map, s.tick) {
        Some((t, r)) => t - s.tick <= 30 && from_centre > r,
        None => false,
    };
    let (mut power, mut health) = (0.0f32, 0.0f32);
    for (pi, def) in e.map.pads.iter().enumerate() {
        if !s.pads[pi].available {
            continue;
        }
        let near = (6 - chebyshev(f.pos, def.pos)).max(0) as f32 / 6.0;
        match def.kind {
            PadKind::Power if f.power_until == NO_BUFF => power = power.max(near),
            PadKind::Health => health = health.max(near * (1.0 - hp)),
            _ => {}
        }
    }
    let idle = f.state == FState::Idle;
    let mut out = [0.0f32; NV];
    out[..BASIC].copy_from_slice(&[
        hp,
        buff as f32 / 50.0,
        b(shot == 0),
        shot.min(6) as f32 / 6.0,
        b(f.cd_dash == 0),
        f.cd_dash.min(12) as f32 / 12.0,
        b(frost == 0),
        b(burst == 0),
        b(idle && f.cd_move == 0),
        b(!idle),
        b(idle && f.cd_move >= 2),
        b(s.areas().iter().any(|a| a.owner as usize != x && e.map.in_blast(a.pos, a.radius, f.pos))),
        b(wall_behind(e, f.pos, o.pos, 2)),
        (s.zone_radius - from_centre).clamp(-3, 5) as f32 / 5.0,
        b(outside_next),
        power,
        health,
        b(s.projectiles().iter().any(|p| p.owner as usize != x)),
        b(d <= 2 && shot == 0),
        b(d <= 2 && (!idle || f.cd_move > 0)),
        b((3..=4).contains(&d) && shot == 0),
        b(d <= 3 && f.cd_dash == 0),
        hp * hp,
    ]);
    if !extended {
        return out;
    }
    // The opponent's cooldowns, as it threatens us.
    let ocd = |pred: &dyn Fn(&AttackKind) -> bool| -> i32 {
        e.rules.attacks.iter().enumerate().filter(|(_, a)| pred(&a.kind)).map(|(i, _)| o.cd_attack[i]).min().unwrap_or(99)
    };
    let their_burst = ocd(&|k| matches!(k, AttackKind::Burst { .. }));
    let push = e.rules.attacks.iter().find_map(|a| if let AttackKind::Burst { push } = a.kind { Some(push) } else { None }).unwrap_or(0);
    let pinned = push > 0 && wall_behind(e, f.pos, o.pos, push);
    let sees = d <= e.rules.sight_radius && e.map.has_line_of_sight(f.pos, o.pos);
    let (dx, dy) = ((f.pos[0] - o.pos[0]) as f64, (f.pos[1] - o.pos[1]) as f64);
    let flight = ((dx * dx + dy * dy).sqrt() / 2.0).ceil() as i32;
    let their_shot = e.rules.attacks.iter().find(|a| matches!(a.kind, AttackKind::Projectile { .. })).map(|a| a.damage).unwrap_or(0);
    let their_hit = if o.power_until == NO_BUFF { their_shot } else { their_shot * 3 / 2 };
    // Who is nearer to a pad that is there or back within 15 ticks: 1 when we are 3 or more nearer.
    let (mut race_power, mut race_health) = (0.0f32, 0.0f32);
    for (pi, def) in e.map.pads.iter().enumerate() {
        if !(s.pads[pi].available || s.pads[pi].respawn_tick - s.tick <= 15) {
            continue;
        }
        let lead = (chebyshev(o.pos, def.pos) - chebyshev(f.pos, def.pos)).clamp(0, 3) as f32 / 3.0;
        match def.kind {
            PadKind::Power if f.power_until == NO_BUFF => race_power = race_power.max(lead),
            PadKind::Health => race_health = race_health.max(lead * (1.0 - hp)),
            _ => {}
        }
    }
    out[BASIC..].copy_from_slice(&[
        // a wall slam on us is there for the taking, or one dash away
        b(pinned && d <= 2 && their_burst <= 1 && o.state == FState::Idle && f.state != FState::Stunned),
        b(pinned && (3..=5).contains(&d) && their_burst <= 2 && o.cd_dash <= 1),
        // our shot is ready and the opponent cannot get out of its way
        b(shot == 0 && sees && (o.state != FState::Idle || (o.cd_move >= flight && o.cd_dash >= flight))),
        b(shot == 0 && sees && d >= 5),
        // one or two of their shots from dead
        b(f.hp <= their_hit),
        b(f.hp <= 2 * their_hit),
        race_power,
        race_health,
    ]);
    out
}

/// The difference of the two fighters' vectors, from `x`'s point of view.
pub fn diff(e: &Engine, s: &State, x: usize) -> [f32; NV] {
    diff_with(e, s, x, true)
}

fn diff_with(e: &Engine, s: &State, x: usize, extended: bool) -> [f32; NV] {
    let (a, b) = (phi(e, s, x, extended), phi(e, s, 1 - x, extended));
    let mut out = [0.0; NV];
    for i in 0..NV {
        out[i] = a[i] - b[i];
    }
    out
}

/// The judgment of the position for fighter `x`, in hp.
pub fn value(e: &Engine, s: &State, x: usize, w: &[f32; NV]) -> f32 {
    let extended = w[BASIC..].iter().any(|v| *v != 0.0);
    diff_with(e, s, x, extended).iter().zip(w.iter()).map(|(a, b)| a * b).sum()
}
