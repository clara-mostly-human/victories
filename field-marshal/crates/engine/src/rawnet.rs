//! A judgment learned from the raw board, one network per map, trained on games of that map
//! (self-play and ladder replays; `rawfit` in the conformance crate).
//!
//! Nothing here says what is good: the inputs are the board as it is. For each fighter: where it
//! stands, its hp, buff, state and every cooldown, the walls in the 5x5 square around it, where
//! each pad is from it and when the pad is back, and the nearest shot and blast coming at it.
//! For the position: the tick, the zone, the offset between the fighters and whether they see
//! each other. A network `g` reads (mine, theirs, position); the judgment for fighter `x`, in hp
//! to come, is g(x, other) - g(other, x), so it flips sign with the point of view by construction.

use crate::geom::{chebyshev, P};
use crate::map::{PadKind, MAX_PADS};
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::Engine;

pub const PATCH: i32 = 2; // 5x5
pub const OWN: usize = 14 + 25 + 4 * MAX_PADS_USED + 6;
pub const MAX_PADS_USED: usize = 4;
pub const CTX: usize = 6;
pub const IN: usize = 2 * OWN + CTX;

fn own(e: &Engine, s: &State, x: usize, out: &mut [f32]) {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let max = e.rules.max_hp as f32;
    let w = e.map.w.max(1) as f32;
    let h = e.map.h.max(1) as f32;
    let mut k = 0;
    let mut put = |v: f32, out: &mut [f32]| {
        out[k] = v;
        k += 1;
    };
    put(f.pos[0] as f32 / w, out);
    put(f.pos[1] as f32 / h, out);
    put(f.hp.max(0) as f32 / max, out);
    put(if f.power_until == NO_BUFF { 0.0 } else { (f.power_until - s.tick).max(0) as f32 / 50.0 }, out);
    put((f.state == FState::Idle) as i32 as f32, out);
    put((f.state == FState::Stunned) as i32 as f32, out);
    put((f.state == FState::Recovering || f.state == FState::Dashing) as i32 as f32, out);
    put(if f.state == FState::Idle { 0.0 } else { (f.state_end - s.tick).clamp(0, 12) as f32 / 12.0 }, out);
    put(f.cd_move.min(4) as f32 / 4.0, out);
    put(f.cd_dash.min(12) as f32 / 12.0, out);
    // the attacks by kind: shot, frost, repulsor
    for want in 0..3 {
        let cd = e.rules.attacks.iter().enumerate().filter(|(_, a)| matches!((&a.kind, want), (AttackKind::Projectile { .. }, 0) | (AttackKind::Area { .. }, 1) | (AttackKind::Burst { .. }, 2))).map(|(i, a)| (f.cd_attack[i], a.cooldown.max(1))).next();
        put(cd.map(|(c, m)| c.min(m) as f32 / m as f32).unwrap_or(1.0), out);
    }
    put(chebyshev(f.pos, e.map.center).min(15) as f32 / 15.0, out);
    for dy in -PATCH..=PATCH {
        for dx in -PATCH..=PATCH {
            let p: P = [f.pos[0] + dx, f.pos[1] + dy];
            let off = p[0] < 0 || p[1] < 0 || p[0] >= e.map.w || p[1] >= e.map.h;
            put((off || e.map.is_wall(p)) as i32 as f32, out);
        }
    }
    for i in 0..MAX_PADS_USED {
        if let Some(pd) = e.map.pads.get(i) {
            put((pd.pos[0] - f.pos[0]) as f32 / w, out);
            put((pd.pos[1] - f.pos[1]) as f32 / h, out);
            put(if s.pads[i].available { 0.0 } else { (s.pads[i].respawn_tick - s.tick).clamp(0, 200) as f32 / 200.0 }, out);
            put(if pd.kind == PadKind::Power { 1.0 } else { -1.0 }, out);
        } else {
            for _ in 0..4 {
                put(0.0, out);
            }
        }
    }
    // the nearest of the other side's shots, and a blast of theirs we stand in
    let shot = s.projectiles().iter().filter(|p| p.owner as usize != x).min_by_key(|p| chebyshev(p.pos, f.pos));
    match shot {
        Some(p) => {
            put((p.pos[0] - f.pos[0]) as f32 / w, out);
            put((p.pos[1] - f.pos[1]) as f32 / h, out);
            put(1.0, out);
        }
        None => {
            put(0.0, out);
            put(0.0, out);
            put(0.0, out);
        }
    }
    let blast = s.areas().iter().filter(|a| a.owner as usize != x && e.map.in_blast(a.pos, a.radius, f.pos)).map(|a| (a.explodes_tick - s.tick).max(0)).min();
    put(blast.is_some() as i32 as f32, out);
    put(blast.map(|t| t.min(10) as f32 / 10.0).unwrap_or(0.0), out);
    put(can_see(&e.rules, &e.map, f.pos, o.pos) as i32 as f32, out);
    debug_assert_eq!(k, OWN);
}

/// The inputs of `g` for fighter `x` as "mine".
pub fn encode(e: &Engine, s: &State, x: usize, out: &mut [f32; IN]) {
    own(e, s, x, &mut out[..OWN]);
    own(e, s, 1 - x, &mut out[OWN..2 * OWN]);
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let c = &mut out[2 * OWN..];
    c[0] = s.tick as f32 / 600.0;
    c[1] = s.zone_radius.clamp(0, 15) as f32 / 15.0;
    c[2] = (o.pos[0] - f.pos[0]) as f32 / 20.0;
    c[3] = (o.pos[1] - f.pos[1]) as f32 / 20.0;
    c[4] = chebyshev(f.pos, o.pos).min(20) as f32 / 20.0;
    c[5] = 1.0;
}

/// The widest hidden layer the network may have.
pub const MAX_H: usize = 128;

/// The hidden units' activation: a fast tanh (rational, exact at 0, within 2% elsewhere,
/// clamped to [-1, 1]); training uses the same function.
#[inline]
pub fn act(z: f32) -> f32 {
    let z = z.clamp(-3.0, 3.0);
    let z2 = z * z;
    z * (27.0 + z2) / (27.0 + 9.0 * z2)
}

/// A two-hidden-layer network with tanh-like units.
#[derive(Clone, Debug)]
pub struct RawNet {
    pub map: String,
    pub h1: usize,
    pub h2: usize,
    /// h1 x IN, then h1 biases, h2 x h1, h2 biases, h2 output weights, output bias
    pub w: Vec<f32>,
}

impl RawNet {
    pub fn params(h1: usize, h2: usize) -> usize {
        h1 * IN + h1 + h2 * h1 + h2 + h2 + 1
    }

    pub fn g(&self, x: &[f32; IN]) -> f32 {
        let (h1, h2) = (self.h1.min(MAX_H), self.h2.min(MAX_H));
        let w = &self.w;
        let mut a1 = [0.0f32; MAX_H];
        for j in 0..h1 {
            let row = &w[j * IN..(j + 1) * IN];
            let mut z = w[self.h1 * IN + j];
            for i in 0..IN {
                z += row[i] * x[i];
            }
            a1[j] = act(z);
        }
        let base2 = self.h1 * IN + self.h1;
        let mut out = w[base2 + self.h2 * self.h1 + self.h2 + self.h2];
        for j in 0..h2 {
            let row = &w[base2 + j * self.h1..base2 + (j + 1) * self.h1];
            let mut z = w[base2 + self.h2 * self.h1 + j];
            for i in 0..h1 {
                z += row[i] * a1[i];
            }
            out += w[base2 + self.h2 * self.h1 + self.h2 + j] * act(z);
        }
        out
    }

    /// The judgment for fighter 0, in hp to come.
    pub fn value0(&self, e: &Engine, s: &State) -> f32 {
        let mut a = [0.0f32; IN];
        let mut b = [0.0f32; IN];
        encode(e, s, 0, &mut a);
        encode(e, s, 1, &mut b);
        self.g(&a) - self.g(&b)
    }

    pub fn load(path: &std::path::Path) -> Option<RawNet> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let h1 = v.get("h1")?.as_u64()? as usize;
        let h2 = v.get("h2")?.as_u64()? as usize;
        if v.get("in")?.as_u64()? as usize != IN {
            return None;
        }
        let w: Vec<f32> = v.get("w")?.as_array()?.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
        if w.len() != Self::params(h1, h2) {
            return None;
        }
        Some(RawNet { map: v.get("map").and_then(|m| m.as_str()).unwrap_or("").to_string(), h1, h2, w })
    }
}

#[allow(dead_code)]
const _: () = assert!(MAX_PADS_USED <= MAX_PADS);
