//! A learned model of which turn a fighter plays: a conditional logit over the turns the search
//! considers (`mcts::actions_with`). Each turn gets features from the position and the turn
//! (its movement class by range band, where it ends up, whether its attack lands if the other
//! side holds); the probability of a turn is softmax(w . features) over the list. The weights
//! are fitted from replays (`turnfit` in the conformance crate): a general model on strong bots,
//! and one per opponent that starts from the general one and moves as far as its own games allow.

use crate::geom::{add, chebyshev, DIR_VEC, P};
use crate::map::PadKind;
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{State, NO_BUFF};
use crate::turn::{AttackIn, DirIn, RawTurn};
use crate::Engine;

/// Movement classes x attack classes x range bands, then the shaped features.
pub const CLASS_BANDS: usize = 28 * 4;
pub const TF: usize = CLASS_BANDS + 14;

pub const SHAPED_NAMES: [&str; 14] = [
    "closer by", "walls next to the tile", "sees them after", "power pad closer by", "onto a power pad",
    "health pad closer by (hurt)", "attack can land", "repulsor slams if they hold", "repulsor hurts if they hold",
    "steps out of a blast", "buffed: closer by", "they are buffed: closer by", "ahead in hp: closer by", "shot from 5+",
];

fn band(d: i32) -> usize {
    if d <= 2 { 0 } else if d <= 4 { 1 } else if d <= 7 { 2 } else { 3 }
}

/// Where fighter `x` stands after the movement part of `t` (the other side holding).
fn dest(e: &Engine, s: &State, x: usize, t: &RawTurn) -> P {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let mut p = f.pos;
    if let DirIn::Dir(d) = t.dash {
        for _ in 0..e.rules.dash_distance {
            let n = add(p, DIR_VEC[d as usize]);
            if e.map.step_blocked(p, d) || n == o.pos {
                break;
            }
            p = n;
        }
    } else if let DirIn::Dir(d) = t.mv {
        let n = add(p, DIR_VEC[d as usize]);
        if !e.map.step_blocked(p, d) && n != o.pos {
            p = n;
        }
    }
    p
}

fn walls_around(e: &Engine, p: P) -> f32 {
    (0..8u8).filter(|d| e.map.step_blocked(p, *d)).count() as f32 / 8.0
}

/// The features of turn `t` for fighter `x` at `s`.
pub fn turn_features(e: &Engine, s: &State, x: usize, t: &RawTurn) -> [f32; TF] {
    let mut out = [0.0f32; TF];
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let d = chebyshev(f.pos, o.pos);
    out[band(d) * 28 + crate::mcts::turn_class(e, s, x, t)] = 1.0;
    let to = dest(e, s, x, t);
    let da = chebyshev(to, o.pos);
    let closer = (d - da).clamp(-3, 3) as f32 / 3.0;
    let k = CLASS_BANDS;
    out[k] = closer;
    out[k + 1] = walls_around(e, to);
    out[k + 2] = can_see(&e.rules, &e.map, to, o.pos) as i32 as f32;
    let max = e.rules.max_hp as f32;
    let hurt = 1.0 - f.hp.max(0) as f32 / max;
    let (mut power_gain, mut on_power, mut health_gain) = (0.0f32, 0.0f32, 0.0f32);
    for (i, pd) in e.map.pads.iter().enumerate() {
        let soon = s.pads[i].available || s.pads[i].respawn_tick - s.tick <= 5;
        if !soon {
            continue;
        }
        let gain = (chebyshev(f.pos, pd.pos) - chebyshev(to, pd.pos)).clamp(-3, 3) as f32 / 3.0;
        match pd.kind {
            PadKind::Power if f.power_until == NO_BUFF => {
                power_gain = power_gain.max(gain);
                if to == pd.pos {
                    on_power = 1.0;
                }
            }
            PadKind::Health => health_gain = health_gain.max(gain * hurt),
            _ => {}
        }
    }
    out[k + 3] = power_gain;
    out[k + 4] = on_power;
    out[k + 5] = health_gain;
    if let AttackIn::Named { attack, .. } = t.attack {
        let a = &e.rules.attacks[attack as usize];
        match a.kind {
            AttackKind::Projectile { .. } => {
                out[k + 6] = can_see(&e.rules, &e.map, to, o.pos) as i32 as f32;
                out[k + 13] = (da >= 5) as i32 as f32;
            }
            AttackKind::Area { range, .. } => {
                out[k + 6] = (da <= range && can_see(&e.rules, &e.map, to, o.pos)) as i32 as f32;
            }
            AttackKind::Burst { .. } => {
                let mut st = *s;
                let mut turns = [RawTurn::IDLE; 2];
                turns[x] = *t;
                let sum = e.step(&mut st, &turns);
                out[k + 7] = sum.slammed[1 - x] as i32 as f32;
                out[k + 8] = (sum.hp_lost[1 - x] > 0 && !sum.slammed[1 - x]) as i32 as f32;
                out[k + 6] = (sum.hp_lost[1 - x] > 0) as i32 as f32;
            }
        }
    }
    let in_blast = |p: P| s.areas().iter().any(|a| a.owner as usize != x && e.map.in_blast(a.pos, a.radius, p));
    out[k + 9] = (in_blast(f.pos) && !in_blast(to)) as i32 as f32;
    out[k + 10] = if f.power_until != NO_BUFF && f.power_until > s.tick { closer } else { 0.0 };
    out[k + 11] = if o.power_until != NO_BUFF && o.power_until > s.tick { closer } else { 0.0 };
    out[k + 12] = ((f.hp - o.hp) as f32 / 50.0).clamp(-1.0, 1.0) * closer;
    out
}

/// Softmax probabilities of `turns` under weights `w`.
pub fn turn_probs(e: &Engine, s: &State, x: usize, turns: &[RawTurn], w: &[f32]) -> Vec<f32> {
    let z: Vec<f32> = turns.iter().map(|t| turn_features(e, s, x, t).iter().zip(w).map(|(a, b)| a * b).sum()).collect();
    let m = z.iter().cloned().fold(f32::MIN, f32::max);
    let ex: Vec<f32> = z.iter().map(|v| (v - m).exp()).collect();
    let sum: f32 = ex.iter().sum();
    ex.iter().map(|v| v / sum.max(1e-12)).collect()
}

/// A fitted turn model, loaded from JSON {"name": ..., "w": [...]}.
#[derive(Clone, Debug)]
pub struct TurnModel {
    pub name: String,
    pub w: Vec<f32>,
}

impl TurnModel {
    pub fn load(path: &std::path::Path) -> Option<TurnModel> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let w: Vec<f32> = v.get("w")?.as_array()?.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
        if w.len() != TF {
            return None;
        }
        Some(TurnModel { name: v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(), w })
    }
}
