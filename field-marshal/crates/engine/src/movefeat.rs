//! Features of a movement decision, for a model of how the ladder's strong bots move
//! (the plan's opponent model, also usable as our own movement prior).
//!
//! `features` describes the tempo situation from the point of view of one fighter that
//! can act and step; `move_class` says what a turn did in the same terms the model
//! predicts: hold, step toward the opponent, away, sideways, or dash.

use crate::geom::{add, chebyshev, octant, DIR_VEC, P};
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::turn::{AttackIn, DirIn, RawTurn};
use crate::Engine;

pub const N: usize = 52;
pub const CLASSES: usize = 5;
pub const CLASS_NAMES: [&str; CLASSES] = ["hold", "toward", "away", "sideways", "dash"];

/// What the turn does: 0 hold, 1 step toward, 2 step away, 3 step sideways, 4 dash.
pub fn move_class(s: &State, x: usize, turn: &RawTurn) -> usize {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if matches!(turn.dash, DirIn::Dir(_)) && f.cd_dash == 0 {
        return 4;
    }
    if let (DirIn::Dir(dir), 0) = (turn.mv, f.cd_move) {
        let d = chebyshev(f.pos, o.pos);
        let after = chebyshev(add(f.pos, DIR_VEC[dir as usize]), o.pos);
        return if after < d { 1 } else if after > d { 2 } else { 3 };
    }
    0
}

/// The five decisions the fitted models predict, in the order `labels` returns them.
pub const TARGETS: [&str; 5] = ["move", "spent", "shot", "frost", "burst"];

/// What `turn` says about each of the five decisions for fighter `x` in state `s`, or
/// `None` where that decision was not open: the move class when its step is ready, whether
/// it dashes when the step is spent, and whether it uses the shot, the frostburst (within
/// range) and the repulsor (within 2 tiles) when ready. All `None` unless it can act and
/// sees its opponent. Used to fit the models on replays and on the search's own games.
pub fn labels(e: &Engine, s: &State, x: usize, turn: &RawTurn) -> [Option<usize>; 5] {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if f.state != FState::Idle || !can_see(&e.rules, &e.map, f.pos, o.pos) {
        return [None; 5];
    }
    let d = chebyshev(f.pos, o.pos);
    let ready = |want: u8| {
        e.rules.attacks.iter().enumerate().find(|(i, a)| {
            f.cd_attack[*i] == 0
                && match (&a.kind, want) {
                    (AttackKind::Projectile { .. }, 0) => true,
                    (AttackKind::Area { range, .. }, 1) => d <= *range,
                    (AttackKind::Burst { .. }, 2) => d <= 2,
                    _ => false,
                }
        }).map(|(i, _)| i)
    };
    let used = |i: usize| matches!(turn.attack, AttackIn::Named { attack, .. } if attack as usize == i) as usize;
    [
        if f.cd_move == 0 { Some(move_class(s, x, turn)) } else { None },
        if f.cd_move != 0 && f.cd_dash == 0 { Some(matches!(turn.dash, DirIn::Dir(_)) as usize) } else { None },
        ready(0).map(used),
        ready(1).map(used),
        ready(2).map(used),
    ]
}

/// Is there a wall (or the map edge) within `reach` tiles of `from`, going straight away from `threat`?
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

/// The situation of fighter `x`. Call only when the opponent is in sight.
pub fn features(e: &Engine, s: &State, x: usize) -> [f32; N] {
    let mut v = [0.0f32; N];
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let cd = |fighter: &crate::state::Fighter, pred: &dyn Fn(&AttackKind) -> bool| -> i32 {
        e.rules.attacks.iter().enumerate().filter(|(_, a)| pred(&a.kind)).map(|(i, _)| fighter.cd_attack[i]).min().unwrap_or(99)
    };
    let shot = |k: &AttackKind| matches!(k, AttackKind::Projectile { .. });
    let area = |k: &AttackKind| matches!(k, AttackKind::Area { .. });
    let burst = |k: &AttackKind| matches!(k, AttackKind::Burst { .. });
    let d = chebyshev(f.pos, o.pos);
    let mut i = 0;
    let mut put = |val: f32| {
        v[i] = val;
        i += 1;
    };
    put(1.0); // 0 bias
    for k in 1..=6 {
        put((d == k) as i32 as f32); // 1..6 range
    }
    put((d >= 7) as i32 as f32); // 7
    let (my_shot, their_shot) = (cd(f, &shot), cd(o, &shot));
    for c in [my_shot, their_shot] {
        put((c == 0) as i32 as f32);
        put((c == 1 || c == 2) as i32 as f32);
        put((c >= 3) as i32 as f32);
    } // 8..13
    put((f.cd_dash == 0) as i32 as f32); // 14
    put((o.cd_dash == 0) as i32 as f32); // 15
    put((o.cd_dash > 6) as i32 as f32); // 16 their dash away for a whole fuse
    put((o.state == FState::Idle && o.cd_move == 0) as i32 as f32); // 17
    put((o.state == FState::Idle && o.cd_move == 1) as i32 as f32); // 18
    put((o.state == FState::Idle && o.cd_move >= 2) as i32 as f32); // 19 slowed
    put((o.state != FState::Idle) as i32 as f32); // 20 cannot act
    put((cd(f, &area) == 0) as i32 as f32); // 21
    put((cd(o, &area) == 0) as i32 as f32); // 22
    put((cd(f, &burst) == 0) as i32 as f32); // 23
    put((cd(o, &burst) == 0) as i32 as f32); // 24
    let max = e.rules.max_hp as f32;
    put(f.hp as f32 / max); // 25
    put(o.hp as f32 / max); // 26
    put((f.hp - o.hp) as f32 / max); // 27
    put((f.power_until != NO_BUFF) as i32 as f32); // 28
    put((o.power_until != NO_BUFF) as i32 as f32); // 29
    let in_area = |who: usize, pos: P| s.areas().iter().any(|b| b.owner as usize != who && e.map.in_blast(b.pos, b.radius, pos));
    put(in_area(x, f.pos) as i32 as f32); // 30 standing in their burst
    put(in_area(1 - x, o.pos) as i32 as f32); // 31 they stand in ours
    put(wall_behind(e, f.pos, o.pos, 2) as i32 as f32); // 32
    put(wall_behind(e, o.pos, f.pos, 2) as i32 as f32); // 33
    put(s.projectiles().iter().any(|p| p.owner as usize != x) as i32 as f32); // 34 their shot in the air
    put(s.projectiles().iter().any(|p| p.owner as usize == x) as i32 as f32); // 35 ours in the air
    // pads: an available power pad within 3 tiles; an available health pad within 4 while hurt
    let mut power_near = false;
    let mut health_near = false;
    for (pi, def) in e.map.pads.iter().enumerate() {
        if !s.pads[pi].available {
            continue;
        }
        let pd = chebyshev(f.pos, def.pos);
        match def.kind {
            crate::map::PadKind::Power => power_near |= pd <= 3,
            crate::map::PadKind::Health => health_near |= pd <= 4 && f.hp < e.rules.max_hp - 20,
        }
    }
    put(power_near as i32 as f32); // 36
    put(health_near as i32 as f32); // 37
    put((chebyshev(f.pos, e.map.center) > s.zone_radius - 1) as i32 as f32); // 38 at the zone's edge or outside
    put(can_see(&e.rules, &e.map, f.pos, o.pos) as i32 as f32); // 39 (1 when called as intended)
    // 40..51: range band x own shot ready x their shot ready, the cells of the plain table
    let band = if d <= 2 { 0 } else if d <= 4 { 1 } else { 2 };
    let cell = band * 4 + ((my_shot == 0) as usize) * 2 + (their_shot == 0) as usize;
    for k in 0..12 {
        put((k == cell) as i32 as f32);
    }
    debug_assert_eq!(i, N);
    v
}

/// The plain table's cell for the same situation (0..12), for a baseline.
pub fn table_cell(v: &[f32; N]) -> usize {
    (40..52).position(|k| v[k] > 0.5).unwrap_or(0)
}
