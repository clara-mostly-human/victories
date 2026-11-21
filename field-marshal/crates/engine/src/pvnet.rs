//! One network read from the raw board with two outputs: how likely each fighter is to win from
//! here, and what each fighter does here. Trained on the search's own conclusions (`aztrain`):
//! the value on who went on to win, the moves on where the search put its visits. It is the
//! search's memory: a kind of position searched once is known the next time it comes up,
//! wherever on the map and whenever in the game, because the inputs describe the position
//! (`rawnet::encode`: distances, cooldowns, statuses, the walls around each fighter), not the
//! path that led to it.
//!
//! A trunk `h` reads (mine, theirs, position) and gives a score `g` and one number per kind of
//! turn. Fighter 0 wins with chance sigmoid(g(0's view) - g(1's view)), so the two fighters'
//! chances add up to one by construction. Fighter x's turns are weighted by the trunk on x's view.
//!
//! A kind of turn is a movement (none, a step in one of 8 directions, a dash in one of 8) with an
//! attack (none, or one of up to three attacks by its place in the rules): 17 x 4 = 68 kinds.

use crate::geom::{add, chebyshev, DIR_VEC};
use crate::observe::can_see;
use crate::rawnet::{act, encode, IN};
use crate::rules::AttackKind;
use crate::state::{FState, State};
use crate::turn::{AttackIn, DirIn, RawTurn};
use crate::Engine;

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

/// The slope of `rawnet::act` at `z`.
#[inline]
pub fn act_d(z: f32) -> f32 {
    if !(-3.0..=3.0).contains(&z) {
        return 0.0;
    }
    let z2 = z * z;
    let (a, b) = (9.0 - z2, 3.0 + z2);
    a * a / (9.0 * b * b)
}

#[inline(always)]
fn axpy(acc: &mut [f32], x: f32, w: &[f32]) {
    for (a, w) in acc.iter_mut().zip(w.iter()) {
        *a += x * *w;
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

pub fn layout(h1: usize, h2: usize) -> Layout {
    let t1 = 0;
    let b1 = t1 + IN * h1;
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
        let l = layout(h1, h2);
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
        PvNet { map: map.to_string(), h1, h2, w }
    }

    /// One pass over one view's inputs.
    pub fn forward(&self, x: &[f32; IN], p: &mut Pass) {
        let (h1, h2) = (self.h1, self.h2);
        let l = layout(h1, h2);
        let w = &self.w;
        let z1 = &mut p.z1[..h1];
        z1.copy_from_slice(&w[l.b1..l.b1 + h1]);
        for (i, &xi) in x.iter().enumerate() {
            if xi != 0.0 {
                axpy(z1, xi, &w[l.t1 + i * h1..l.t1 + (i + 1) * h1]);
            }
        }
        for j in 0..h1 {
            p.a1[j] = act(p.z1[j]);
        }
        let z2 = &mut p.z2[..h2];
        z2.copy_from_slice(&w[l.b2..l.b2 + h2]);
        for i in 0..h1 {
            axpy(z2, p.a1[i], &w[l.t2 + i * h2..l.t2 + (i + 1) * h2]);
        }
        for j in 0..h2 {
            p.a2[j] = act(p.z2[j]);
        }
        p.g = w[l.vb] + dot(&w[l.vw..l.vw + h2], &p.a2[..h2]);
        p.logits.copy_from_slice(&w[l.pb..l.pb + CLASSES]);
        for j in 0..h2 {
            axpy(&mut p.logits, p.a2[j], &w[l.tp + j * CLASSES..l.tp + (j + 1) * CLASSES]);
        }
    }

    /// Adds to `grad` the slope of a loss with respect to every weight, given the loss's slope
    /// with respect to this view's score (`dg`) and to its number for each kind of turn.
    pub fn backward(&self, x: &[f32; IN], p: &Pass, dg: f32, dlogits: &[f32; CLASSES], grad: &mut [f32]) {
        let (h1, h2) = (self.h1, self.h2);
        let l = layout(h1, h2);
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
        for (i, &xi) in x.iter().enumerate() {
            if xi != 0.0 {
                axpy(&mut grad[l.t1 + i * h1..l.t1 + (i + 1) * h1], xi, &dz1[..h1]);
            }
        }
    }

    /// Reads a position: fighter 0's result to come and both fighters' turn numbers.
    pub fn eval(&self, e: &Engine, s: &State) -> Eval {
        let mut x = [0.0f32; IN];
        let mut p = Pass::default();
        encode(e, s, 0, &mut x);
        self.forward(&x, &mut p);
        let (g0, l0) = (p.g, p.logits);
        encode(e, s, 1, &mut x);
        self.forward(&x, &mut p);
        Eval { v0: ((g0 - p.g) * 0.5).tanh(), logits: [l0, p.logits] }
    }

    pub fn save(&self, path: &std::path::Path, note: &serde_json::Value) -> std::io::Result<()> {
        let json = serde_json::json!({ "kind": "pvnet", "map": self.map, "h1": self.h1, "h2": self.h2, "in": IN, "classes": CLASSES, "note": note, "w": self.w });
        std::fs::write(path, serde_json::to_string(&json).unwrap_or_default())
    }

    pub fn load(path: &std::path::Path) -> Option<PvNet> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let h1 = v.get("h1")?.as_u64()? as usize;
        let h2 = v.get("h2")?.as_u64()? as usize;
        if v.get("in")?.as_u64()? as usize != IN || v.get("classes")?.as_u64()? as usize != CLASSES || h1 > MAX_H || h2 > MAX_H {
            return None;
        }
        let w: Vec<f32> = v.get("w")?.as_array()?.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
        if w.len() != layout(h1, h2).len {
            return None;
        }
        Some(PvNet { map: v.get("map").and_then(|m| m.as_str()).unwrap_or("").to_string(), h1, h2, w })
    }
}

/// One saved position of a self-play game, as `azgen` writes it and `aztrain` reads it:
/// both views' inputs (as `rawnet::encode`, times 100), per fighter the share of the search's
/// visits on each kind of turn (times 255) and which kinds it could play, the search's own value
/// for fighter 0 (times 127; `has_q` 0 when the search does not count in wins), and who won.
pub const REC: usize = 2 * IN + 2 * CLASSES + 2 * 9 + 6;

pub struct Rec {
    pub a: [i8; IN],
    pub b: [i8; IN],
    pub pol: [[u8; CLASSES]; 2],
    pub legal: [[bool; CLASSES]; 2],
    pub q: f32,
    pub has_q: bool,
    /// 1 fighter 0 won, -1 fighter 1 won, 0 a draw
    pub z: i8,
    pub tick: u16,
}

impl Rec {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend(self.a.iter().map(|v| *v as u8));
        out.extend(self.b.iter().map(|v| *v as u8));
        for x in 0..2 {
            out.extend_from_slice(&self.pol[x]);
        }
        for x in 0..2 {
            let mut bits = [0u8; 9];
            for c in 0..CLASSES {
                if self.legal[x][c] {
                    bits[c / 8] |= 1 << (c % 8);
                }
            }
            out.extend_from_slice(&bits);
        }
        out.push((self.q.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8);
        out.push(self.has_q as u8);
        out.push(self.z as u8);
        out.extend_from_slice(&self.tick.to_le_bytes());
        out.push(0);
    }

    pub fn read(b: &[u8]) -> Rec {
        let mut r = Rec { a: [0; IN], b: [0; IN], pol: [[0; CLASSES]; 2], legal: [[false; CLASSES]; 2], q: 0.0, has_q: false, z: 0, tick: 0 };
        for i in 0..IN {
            r.a[i] = b[i] as i8;
            r.b[i] = b[IN + i] as i8;
        }
        let mut k = 2 * IN;
        for x in 0..2 {
            r.pol[x].copy_from_slice(&b[k..k + CLASSES]);
            k += CLASSES;
        }
        for x in 0..2 {
            for c in 0..CLASSES {
                r.legal[x][c] = b[k + c / 8] >> (c % 8) & 1 == 1;
            }
            k += 9;
        }
        r.q = b[k] as i8 as f32 / 127.0;
        r.has_q = b[k + 1] != 0;
        r.z = b[k + 2] as i8;
        r.tick = u16::from_le_bytes([b[k + 3], b[k + 4]]);
        r
    }
}

/// Working room for `loss_grad`.
#[derive(Default)]
pub struct Scratch {
    pa: Pass,
    pb: Pass,
}

/// What one saved position costs under the network, for the trainer's reports.
#[derive(Default, Clone, Copy)]
pub struct Loss {
    /// how far the chance to win is from the target (cross-entropy)
    pub value: f32,
    /// the game had a winner, and whether the network's favourite was that winner
    pub decided: bool,
    pub value_hit: bool,
    /// summed over the fighters that had a choice: cross-entropy between the search's visit
    /// shares and the network's turn shares, the visit shares' own entropy, how many fighters,
    /// and for how many the network's first choice was the search's
    pub policy: f32,
    pub policy_floor: f32,
    pub policy_n: u32,
    pub top_hit: u32,
}

/// The value target for a saved position, as fighter 0's chance to win: the game's result,
/// mixed (`lambda`) with the search's own value where the search counted in wins.
pub fn value_target(r: &Rec, lambda: f32) -> f32 {
    let z = r.z as f32;
    let mix = if r.has_q { (1.0 - lambda) * z + lambda * r.q } else { z };
    0.5 * (1.0 + mix)
}

/// The loss of one saved position, and (with `grad`) its slope added for every weight:
/// cross-entropy of the chance to win against `value_target`, plus `pw` times the cross-entropy
/// of each fighter's turn shares against the search's visit shares.
pub fn loss_grad(net: &PvNet, r: &Rec, lambda: f32, pw: f32, sc: &mut Scratch, grad: Option<&mut [f32]>) -> Loss {
    let mut xa = [0.0f32; IN];
    let mut xb = [0.0f32; IN];
    for i in 0..IN {
        xa[i] = r.a[i] as f32 / 100.0;
        xb[i] = r.b[i] as f32 / 100.0;
    }
    net.forward(&xa, &mut sc.pa);
    net.forward(&xb, &mut sc.pb);
    let d = sc.pa.g - sc.pb.g;
    let p = 1.0 / (1.0 + (-d).exp());
    let t = value_target(r, lambda);
    let eps = 1e-7f32;
    let mut out = Loss { value: -(t * (p + eps).ln() + (1.0 - t) * (1.0 - p + eps).ln()), decided: r.z != 0, value_hit: (d > 0.0) == (r.z > 0), ..Default::default() };
    let mut dl = [[0.0f32; CLASSES]; 2];
    for f in 0..2 {
        let logits = if f == 0 { &sc.pa.logits } else { &sc.pb.logits };
        let legal = r.legal[f].iter().filter(|l| **l).count();
        let total: u32 = r.pol[f].iter().map(|v| *v as u32).sum();
        if legal < 2 || total == 0 {
            continue;
        }
        let top = (0..CLASSES).filter(|c| r.legal[f][*c]).map(|c| logits[c]).fold(f32::MIN, f32::max);
        let mut sum = 0.0f32;
        let mut sm = [0.0f32; CLASSES];
        for c in 0..CLASSES {
            if r.legal[f][c] {
                sm[c] = (logits[c] - top).exp();
                sum += sm[c];
            }
        }
        let (mut best_net, mut best_pi) = ((f32::MIN, 0), (0u8, 0));
        for c in 0..CLASSES {
            if !r.legal[f][c] {
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
        net.backward(&xa, &sc.pa, p - t, &dl[0], grad);
        net.backward(&xb, &sc.pb, t - p, &dl[1], grad);
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
    pad: [[usize; crate::rawnet::MAX_PADS_USED]; 4],
    /// for each image, the direction each direction becomes
    dir: [[usize; 8]; 4],
}

impl Sym {
    pub fn of(e: &Engine) -> Sym {
        let (w, h) = (e.map.w, e.map.h);
        let flip = |k: usize, p: [i32; 2]| [if k & 1 != 0 { w - 1 - p[0] } else { p[0] }, if k & 2 != 0 { h - 1 - p[1] } else { p[1] }];
        let mut s = Sym { w, h, ok: [true; 4], pad: [[0; crate::rawnet::MAX_PADS_USED]; 4], dir: [[0; 8]; 4] };
        for k in 0..4 {
            for y in 0..h {
                for x in 0..w {
                    if e.map.is_wall([x, y]) != e.map.is_wall(flip(k, [x, y])) {
                        s.ok[k] = false;
                    }
                }
            }
            if e.map.pads.len() > crate::rawnet::MAX_PADS_USED {
                s.ok[k] = k == 0;
            }
            for i in 0..crate::rawnet::MAX_PADS_USED {
                s.pad[k][i] = i;
                if let Some(pd) = e.map.pads.get(i) {
                    // the pad whose image stands on pad i
                    match e.map.pads.iter().position(|q| flip(k, q.pos) == pd.pos && q.kind == pd.kind) {
                        Some(j) if j < crate::rawnet::MAX_PADS_USED => s.pad[k][i] = j,
                        _ => s.ok[k] = false,
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

    fn view(&self, k: usize, v: &[i8; IN]) -> [i8; IN] {
        use crate::rawnet::{MAX_PADS_USED, OWN, PATCH};
        let (mx, my) = (k & 1 != 0, k & 2 != 0);
        let mut out = *v;
        let side = (2 * PATCH + 1) as usize;
        let pos = |q: i8, n: i32| -> i8 {
            let x = (q as f32 * n as f32 / 100.0).round() as i32;
            (((n - 1 - x) as f32 / n as f32) * 100.0).round().clamp(-127.0, 127.0) as i8
        };
        for b in 0..2 {
            let o = b * OWN;
            if mx {
                out[o] = pos(v[o], self.w);
            }
            if my {
                out[o + 1] = pos(v[o + 1], self.h);
            }
            // the walls around the fighter
            let patch = o + 14;
            for y in 0..side {
                for x in 0..side {
                    let (sx, sy) = (if mx { side - 1 - x } else { x }, if my { side - 1 - y } else { y });
                    out[patch + y * side + x] = v[patch + sy * side + sx];
                }
            }
            // the pads, each in the slot of the pad its image stands on
            let pads = patch + side * side;
            for i in 0..MAX_PADS_USED {
                let j = self.pad[k][i];
                for f in 0..4 {
                    out[pads + 4 * i + f] = v[pads + 4 * j + f];
                }
                if mx {
                    out[pads + 4 * i] = out[pads + 4 * i].saturating_neg();
                }
                if my {
                    out[pads + 4 * i + 1] = out[pads + 4 * i + 1].saturating_neg();
                }
            }
            // the nearest shot
            let shot = pads + 4 * MAX_PADS_USED;
            if mx {
                out[shot] = v[shot].saturating_neg();
            }
            if my {
                out[shot + 1] = v[shot + 1].saturating_neg();
            }
        }
        // where the other fighter is from here
        if mx {
            out[2 * OWN + 2] = v[2 * OWN + 2].saturating_neg();
        }
        if my {
            out[2 * OWN + 3] = v[2 * OWN + 3].saturating_neg();
        }
        out
    }

    /// The saved position `r` in image `k`.
    pub fn image(&self, k: usize, r: &Rec) -> Rec {
        let mut out = Rec { a: self.view(k, &r.a), b: self.view(k, &r.b), pol: [[0; CLASSES]; 2], legal: [[false; CLASSES]; 2], q: r.q, has_q: r.has_q, z: r.z, tick: r.tick };
        for f in 0..2 {
            for c in 0..CLASSES {
                let c2 = self.class(k, c);
                out.pol[f][c2] = r.pol[f][c];
                out.legal[f][c2] = r.legal[f][c];
            }
        }
        out
    }
}
