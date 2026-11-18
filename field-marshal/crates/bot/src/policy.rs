//! The policy: a one-tick matrix search over a safe heuristic.
//!
//! Every turn we could take is given its own merits by the heuristic: staying inside the
//! zone, getting out of what is already in the air (planned over a frostburst's whole
//! fuse), keeping off walls a repulsor could slam us into, taking pickups when they are
//! cheap. With the opponent in view the exchange itself is then solved as a matrix game:
//! each of our turns against each movement the opponent could make, played on the exact
//! engine and a few ticks on, solved by regret matching (playbook "matrix-1"). Out of
//! view, or with `search` off, the turn with the best worst case is taken ("safe-2").
//!
//! All numbers live in [`Playbook`], so a playbook for a named opponent can change them.
//! They were tuned in the arena and then confirmed against other bots. Two findings are
//! built into the defaults and deserve suspicion later, against stronger opponents: the
//! search does not model the opponent's attacks (doing so made it far too timid against
//! everything tried so far), and it plays its most likely turn instead of sampling.

use crate::tracker::Tracker;
use serde_json::Value;
use std::collections::VecDeque;
use victris_engine::geom::{add, chebyshev, octant, DIR_VEC, P};
use victris_engine::map::PadKind;
use victris_engine::observe::{can_see, View};
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, State, NO_BUFF};
use victris_engine::turn::RawTurn;
use victris_engine::Engine;

/// A named set of policy settings. One is chosen per match, by opponent.
#[derive(Clone, Debug)]
pub struct Playbook {
    pub name: String,
    /// what a ranged shot must be worth before it is fired (it costs the cooldown)
    pub fire_cost: f64,
    pub repulsor_cost: f64,
    pub frost_cost: f64,
    /// value of casting a frostburst on a visible opponent (it slows and forces a move)
    pub frost_value: f64,
    /// extra value of stunning, on top of the damage (a stun sets up a sure shot)
    pub stun_bonus: f64,
    /// weight of the average outcome next to the worst case
    pub average_weight: f64,
    /// weight of what the opponent could do to us if it guessed our tile
    pub threat_weight: f64,
    /// cost of stepping while the opponent is close (the step is our dodge)
    pub step_cost_near: f64,
    pub dash_cost: f64,
    /// distance we like to keep from a visible opponent
    pub preferred_distance: f64,
    pub distance_weight: f64,
    /// pull toward the current goal tile, per tile of walking distance
    pub goal_weight: f64,
    /// go for a health pad when this much hp is missing
    pub health_pad_missing_hp: i32,
    /// weight of damage already in the air that the turn leaves us unable to dodge
    pub incoming_weight: f64,
    /// cost of stepping within four tiles of an opponent whose shot is ready
    pub step_cost_under_fire: f64,
    /// choose turns by solving the tick as a matrix game (our turns against theirs)
    /// instead of taking the best worst case; only used while the opponent is in view
    pub search: bool,
    /// ticks each cell of the matrix is played forward, both sides only dodging
    pub rollout_ticks: i32,
    /// search: include the opponent's attacks among its options
    pub model_attacks: bool,
    /// search: play from the mixed strategy (true) or always its most likely turn (false)
    pub sample: bool,
    /// search: consider shots aimed where the opponent could step
    pub lead_shots: bool,
}

impl Default for Playbook {
    fn default() -> Self {
        Playbook {
            name: "matrix-1".into(),
            fire_cost: 3.0,
            repulsor_cost: 1.0,
            frost_cost: 0.5,
            frost_value: 2.5,
            stun_bonus: 8.0,
            average_weight: 0.15,
            threat_weight: 0.0,
            step_cost_near: 0.3,
            dash_cost: 10.0,
            preferred_distance: 3.5,
            distance_weight: 0.4,
            goal_weight: 0.7,
            health_pad_missing_hp: 10,
            incoming_weight: 1.0,
            step_cost_under_fire: 0.3,
            search: true,
            rollout_ticks: 4,
            model_attacks: false,
            sample: false,
            lead_shots: true,
        }
    }
}

impl Playbook {
    /// The default playbook with any fields in `v` replaced.
    pub fn from_json(name: &str, v: &Value) -> Playbook {
        let mut p = Playbook { name: name.to_string(), ..Playbook::default() };
        let f = |k: &str, slot: &mut f64| {
            if let Some(x) = v.get(k).and_then(Value::as_f64) {
                *slot = x;
            }
        };
        f("fire_cost", &mut p.fire_cost);
        f("repulsor_cost", &mut p.repulsor_cost);
        f("frost_cost", &mut p.frost_cost);
        f("frost_value", &mut p.frost_value);
        f("stun_bonus", &mut p.stun_bonus);
        f("average_weight", &mut p.average_weight);
        f("threat_weight", &mut p.threat_weight);
        f("step_cost_near", &mut p.step_cost_near);
        f("dash_cost", &mut p.dash_cost);
        f("preferred_distance", &mut p.preferred_distance);
        f("distance_weight", &mut p.distance_weight);
        f("goal_weight", &mut p.goal_weight);
        f("incoming_weight", &mut p.incoming_weight);
        f("step_cost_under_fire", &mut p.step_cost_under_fire);
        if let Some(x) = v.get("health_pad_missing_hp").and_then(Value::as_i64) {
            p.health_pad_missing_hp = x as i32;
        }
        if let Some(x) = v.get("search").and_then(Value::as_bool) {
            p.search = x;
        }
        for (key, slot) in [("model_attacks", &mut p.model_attacks), ("sample", &mut p.sample), ("lead_shots", &mut p.lead_shots)] {
            if let Some(x) = v.get(key).and_then(Value::as_bool) {
                *slot = x;
            }
        }
        if let Some(x) = v.get("rollout_ticks").and_then(Value::as_i64) {
            p.rollout_ticks = x as i32;
        }
        p
    }
}

pub struct Decision {
    pub turn: RawTurn,
    /// engine ticks simulated to reach it
    pub sims: u32,
    pub score: f64,
    /// short label of why, for the log
    pub note: &'static str,
}

/// Walking distance from every tile to `goal` (8 directions, the engine's corner rule).
fn distances(e: &Engine, goal: P) -> Vec<i32> {
    let (w, h) = (e.map.w, e.map.h);
    let mut dist = vec![i32::MAX; (w * h) as usize];
    if e.map.is_wall(goal) {
        return dist;
    }
    let mut q = VecDeque::new();
    dist[(goal[1] * w + goal[0]) as usize] = 0;
    q.push_back(goal);
    while let Some(p) = q.pop_front() {
        let d = dist[(p[1] * w + p[0]) as usize];
        for dir in 0..8u8 {
            // Walking is symmetric under the corner rule, so searching outward from the goal is fine.
            if e.map.step_blocked(p, dir) {
                continue;
            }
            let n = add(p, DIR_VEC[dir as usize]);
            let i = (n[1] * w + n[0]) as usize;
            if dist[i] == i32::MAX {
                dist[i] = d + 1;
                q.push_back(n);
            }
        }
    }
    dist
}

struct Kit {
    ranged: Option<u8>,
    frost: Option<(u8, i32)>,
    repulsor: Option<u8>,
}

fn kit(e: &Engine) -> Kit {
    let mut k = Kit { ranged: None, frost: None, repulsor: None };
    for (i, a) in e.rules.attacks.iter().enumerate() {
        match a.kind {
            AttackKind::Projectile { .. } if k.ranged.is_none() => k.ranged = Some(i as u8),
            AttackKind::Area { range, .. } if k.frost.is_none() => k.frost = Some((i as u8, range)),
            AttackKind::Burst { .. } if k.repulsor.is_none() => k.repulsor = Some(i as u8),
            _ => {}
        }
    }
    k
}

/// One simulated tick's outcome for us: positive is good.
fn outcome(e: &Engine, before: &State, me: usize, turns: &[RawTurn; 2], pb: &Playbook, sims: &mut u32) -> f64 {
    let mut s = *before;
    e.step(&mut s, turns);
    *sims += 1;
    let (my0, en0) = (before.fighters[me], before.fighters[1 - me]);
    let mut v = (en0.hp - s.fighters[1 - me].hp) as f64 - (my0.hp - s.fighters[me].hp) as f64;
    if s.fighters[1 - me].state == FState::Stunned && en0.state != FState::Stunned {
        v += pb.stun_bonus;
    }
    if s.fighters[me].state == FState::Stunned && my0.state != FState::Stunned {
        v -= pb.stun_bonus;
    }
    if !s.fighters[1 - me].alive {
        v += 1000.0;
    }
    if !s.fighters[me].alive {
        v -= 1000.0;
    }
    // While the opponent cannot act, our shots already in the air land for sure: count them.
    let mut k = 0;
    while s.result.is_none() && k < 3 && s.fighters[1 - me].state != FState::Idle {
        let hp = s.fighters[1 - me].hp;
        e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
        *sims += 1;
        v += (hp - s.fighters[1 - me].hp) as f64;
        if !s.fighters[1 - me].alive {
            v += 1000.0;
        }
        k += 1;
    }
    v
}

/// One cell of the matrix game: both turns played on the exact engine, then a few ticks
/// in which each side only steps out of what would hit it. The value is hp taken from
/// the opponent minus hp lost, with a bonus for stuns and a large one for a kill.
fn cell_value(e: &Engine, before: &State, me: usize, turns: &[RawTurn; 2], pb: &Playbook, sims: &mut u32) -> f64 {
    let mut s = *before;
    e.step(&mut s, turns);
    *sims += 1;
    let (my0, en0) = (before.fighters[me], before.fighters[1 - me]);
    let mut v = 0.0;
    if s.fighters[1 - me].state == FState::Stunned && en0.state != FState::Stunned {
        v += pb.stun_bonus;
    }
    if s.fighters[me].state == FState::Stunned && my0.state != FState::Stunned {
        v -= pb.stun_bonus;
    }
    for _ in 0..pb.rollout_ticks {
        if s.result.is_some() {
            break;
        }
        let base = s;
        let sum = e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
        *sims += 1;
        if sum.hp_lost == [0, 0] {
            continue;
        }
        // Someone would be hit standing still: let whoever can step look for a way out.
        let mut dodge = [RawTurn::IDLE, RawTurn::IDLE];
        let mut any = false;
        for x in 0..2 {
            let f = &base.fighters[x];
            if sum.hp_lost[x] == 0 || f.state != FState::Idle || f.cd_move != 0 {
                continue;
            }
            for d in 0..8u8 {
                if e.map.step_blocked(f.pos, d) {
                    continue;
                }
                let mut trial = base;
                let mut t2 = dodge;
                t2[x] = RawTurn::step(d);
                let r = e.step(&mut trial, &t2);
                *sims += 1;
                if r.hp_lost[x] == 0 {
                    dodge[x] = RawTurn::step(d);
                    any = true;
                    break;
                }
            }
        }
        if any {
            s = base;
            e.step(&mut s, &dodge);
            *sims += 1;
        }
    }
    v += (en0.hp - s.fighters[1 - me].hp) as f64 - (my0.hp - s.fighters[me].hp) as f64;
    if !s.fighters[1 - me].alive {
        v += 1000.0;
    }
    if !s.fighters[me].alive {
        v -= 1000.0;
    }
    v
}

/// Solves a zero-sum matrix game by regret matching. `a[m * cols + e]` is the row
/// player's payoff. Returns the row player's average strategy and the game's value.
fn solve_matrix(a: &[f64], rows: usize, cols: usize, iters: usize) -> (Vec<f64>, f64) {
    let mut rr = vec![0.0f64; rows];
    let mut rc = vec![0.0f64; cols];
    let mut avg_r = vec![0.0f64; rows];
    let mut avg_c = vec![0.0f64; cols];
    let mut sr = vec![0.0f64; rows];
    let mut sc = vec![0.0f64; cols];
    let mut ur = vec![0.0f64; rows];
    let mut uc = vec![0.0f64; cols];
    let strategy = |regret: &[f64], out: &mut [f64]| {
        let total: f64 = regret.iter().sum();
        if total > 0.0 {
            for (o, r) in out.iter_mut().zip(regret) {
                *o = r / total;
            }
        } else {
            out.fill(1.0 / regret.len() as f64);
        }
    };
    for it in 0..iters {
        strategy(&rr, &mut sr);
        strategy(&rc, &mut sc);
        ur.fill(0.0);
        uc.fill(0.0);
        for m in 0..rows {
            let row = &a[m * cols..(m + 1) * cols];
            let mut u = 0.0;
            for e in 0..cols {
                u += sc[e] * row[e];
                uc[e] += sr[m] * row[e];
            }
            ur[m] = u;
        }
        let v: f64 = sr.iter().zip(&ur).map(|(p, u)| p * u).sum();
        let w = (it + 1) as f64; // later iterations count for more
        for m in 0..rows {
            rr[m] = (rr[m] + ur[m] - v).max(0.0);
            avg_r[m] += w * sr[m];
        }
        for e in 0..cols {
            rc[e] = (rc[e] + v - uc[e]).max(0.0);
            avg_c[e] += w * sc[e];
        }
    }
    let tr: f64 = avg_r.iter().sum();
    let tc: f64 = avg_c.iter().sum();
    avg_r.iter_mut().for_each(|x| *x /= tr);
    avg_c.iter_mut().for_each(|x| *x /= tc);
    let mut value = 0.0;
    for m in 0..rows {
        for e in 0..cols {
            value += avg_r[m] * avg_c[e] * a[m * cols + e];
        }
    }
    (avg_r, value)
}

pub fn decide(t: &Tracker, view: &View, pb: &Playbook) -> Decision {
    let idle = Decision { turn: RawTurn::IDLE, sims: 0, score: 0.0, note: "cannot act" };
    let Some(s) = t.belief else { return idle };
    let e = &t.engine;
    let me = t.me;
    let my = s.fighters[me];
    let en = s.fighters[1 - me];
    if my.state != FState::Idle {
        return idle;
    }
    let k = kit(e);
    let visible = view.enemy.is_some();
    let dist_now = chebyshev(my.pos, en.pos);

    // ---- our options: a movement (or none), then an attack (or none)
    let mut moves: Vec<(RawTurn, P, u8)> = vec![(RawTurn::IDLE, my.pos, 0)]; // (turn, tile we expect to end on, 0 stay / 1 step / 2 dash)
    for (d, tile) in &view.moves {
        moves.push((RawTurn::step(*d), *tile, 1));
    }
    for d in &view.dashes {
        moves.push((RawTurn::dash(d.dir), d.tile, 2));
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Atk {
        None,
        Ranged,
        Frost,
        Repulsor,
    }
    let mut attacks = vec![Atk::None];
    if visible {
        if k.ranged.map(|a| my.cd_attack[a as usize] == 0).unwrap_or(false) {
            attacks.push(Atk::Ranged);
        }
        if k.repulsor.map(|a| my.cd_attack[a as usize] == 0).unwrap_or(false) && dist_now <= 2 {
            attacks.push(Atk::Repulsor);
        }
        if let Some((a, range)) = k.frost {
            if my.cd_attack[a as usize] == 0 && dist_now <= range + 1 && dist_now >= 2 {
                attacks.push(Atk::Frost);
            }
        }
    }

    // ---- the opponent's possible movements this tick, by what we believe it has ready
    let mut replies: Vec<RawTurn> = vec![RawTurn::IDLE];
    if visible && en.state == FState::Idle {
        for d in 0..8u8 {
            if en.cd_move == 0 && !e.map.step_blocked(en.pos, d) {
                replies.push(RawTurn::step(d));
            }
            if en.cd_dash == 0 {
                replies.push(RawTurn::dash(d));
            }
        }
    }
    let en_ranged = visible && en.state == FState::Idle && k.ranged.map(|a| en.cd_attack[a as usize] == 0).unwrap_or(false);
    let en_repulsor = visible && en.state == FState::Idle && k.repulsor.map(|a| en.cd_attack[a as usize] == 0).unwrap_or(false);

    // ---- where we want to be
    let centre = e.map.center;
    let zone_now = s.zone_radius;
    let outside = |p: P, r: i32| chebyshev(p, centre) > r;
    let mut goal: Option<P> = None;
    let mut goal_note = "hold";
    if outside(my.pos, zone_now) || view.next_shrink.map(|(r, _)| outside(my.pos, r)).unwrap_or(false) {
        goal = Some(centre);
        goal_note = "zone";
    }
    if goal.is_none() {
        let missing = e.rules.max_hp - my.hp;
        let mut best: Option<(i32, P, &'static str)> = None;
        for (i, def) in e.map.pads.iter().enumerate() {
            let pad = s.pads[i];
            let soon = pad.available || pad.respawn_tick - s.tick <= 3;
            let wanted = match def.kind {
                PadKind::Health => missing >= pb.health_pad_missing_hp,
                PadKind::Power => my.power_until == NO_BUFF,
            };
            let d = chebyshev(my.pos, def.pos);
            let reach = if def.kind == PadKind::Health { 10 } else { 7 };
            // Do not race the opponent to a pad it is clearly closer to.
            let contested = visible && chebyshev(en.pos, def.pos) + 1 < d;
            if soon && wanted && d <= reach && !contested && !outside(def.pos, zone_now) && best.map(|(bd, _, _)| d < bd).unwrap_or(true) {
                best = Some((d, def.pos, if def.kind == PadKind::Health { "health pad" } else { "power pad" }));
            }
        }
        if let Some((_, p, note)) = best {
            goal = Some(p);
            goal_note = note;
        }
    }
    if goal.is_none() && !visible {
        // Head for where the opponent may be: the middle of the tiles it could be on.
        let (w, mut sx, mut sy, mut n) = (e.map.w, 0i64, 0i64, 0i64);
        for (i, b) in t.possible.iter().enumerate() {
            if *b {
                sx += (i as i32 % w) as i64;
                sy += (i as i32 / w) as i64;
                n += 1;
            }
        }
        let mid = if n > 0 { [(sx / n) as i32, (sy / n) as i32] } else { en.pos };
        let mut target = t.enemy_last_seen.map(|(_, p)| p).unwrap_or(mid);
        if n > 0 && !t.possible[(target[1] * w + target[0]) as usize] {
            target = mid;
        }
        if e.map.is_wall(target) || outside(target, zone_now) {
            target = centre;
        }
        goal = Some(target);
        goal_note = "seek";
    }
    let goal_dist = goal.map(|g| distances(e, g));
    let walk = |p: P| -> f64 {
        match &goal_dist {
            Some(d) => {
                let x = d[(p[1] * e.map.w + p[0]) as usize];
                if x == i32::MAX {
                    60.0
                } else {
                    x as f64
                }
            }
            None => 0.0,
        }
    };

    // ---- score every turn
    let mut sims = 0u32;
    let mut best: Option<(f64, RawTurn, &'static str)> = None;
    // A little seeded noise breaks ties differently each tick, so we do not pace in a loop.
    let mut noise_state = (t.seed as u64 ^ 0x9e37_79b9_7f4a_7c15).wrapping_mul(s.tick as u64 + 1) | 1;
    let mut noise = || {
        noise_state ^= noise_state << 13;
        noise_state ^= noise_state >> 7;
        noise_state ^= noise_state << 17;
        (noise_state % 1000) as f64 / 1000.0 * 0.05
    };
    let turns = |mine: RawTurn, theirs: RawTurn| if me == 0 { [mine, theirs] } else { [theirs, mine] };
    // With the search on and the opponent in view, turns are collected here and chosen
    // together as a matrix game: (turn, its own merits apart from the exchange, label).
    let matrix = pb.search && visible;
    let mut cands: Vec<(RawTurn, f64, &'static str)> = Vec::new();

    for (mv, dest, kind) in &moves {
        // Static terms for the tile we would end on.
        let mut place = 0.0;
        let can_step_next = *kind == 0 && my.cd_move <= 1;
        // What is already in the air: after this movement, how much of it can we still get
        // out of the way of? Play the next tick every way we could, then let it all land.
        {
            let mut after = s;
            e.step(&mut after, &turns(*mv, RawTurn::IDLE));
            sims += 1;
            let mut least = i32::MAX;
            if after.result.is_none() {
                // Escape plans: stand still, or keep walking one way whenever a step is
                // ready. Seven ticks covers a frostburst's whole fuse, slow included.
                for plan in 0..9u8 {
                    let mut f = after;
                    let mut lost = 0;
                    for _ in 0..7 {
                        if f.result.is_some() {
                            break;
                        }
                        let fm = &f.fighters[me];
                        let go = plan < 8 && fm.state == FState::Idle && fm.cd_move == 0 && !e.map.step_blocked(fm.pos, plan);
                        let t = if go { RawTurn::step(plan) } else { RawTurn::IDLE };
                        lost += e.step(&mut f, &turns(t, RawTurn::IDLE)).hp_lost[me];
                        sims += 1;
                    }
                    if f.result.is_some() && !f.fighters[me].alive {
                        lost += 1000;
                    }
                    least = least.min(lost);
                    if least == 0 {
                        break;
                    }
                }
            } else {
                least = 0;
            }
            place -= pb.incoming_weight * least as f64;
        }
        if outside(*dest, zone_now) {
            place -= 12.0;
        }
        if let Some((r, in_ticks)) = view.next_shrink {
            if outside(*dest, r) {
                place -= 6.0 * (1.0 - in_ticks as f64 / 25.0).max(0.2);
            }
        }
        place -= pb.goal_weight * walk(*dest);
        if *kind == 2 {
            place -= pb.dash_cost;
        }
        if visible {
            let d = chebyshev(*dest, en.pos);
            if goal.is_none() {
                place -= pb.distance_weight * (d as f64 - pb.preferred_distance).abs().min(6.0);
            }
            // Inside four tiles a shot arrives before a spent step comes back, so while the
            // opponent's shot is ready the step is our dodge: spend it only for a reason.
            if *kind == 1 && d <= 4 {
                let shot_ready = k.ranged.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false);
                place -= if shot_ready { pb.step_cost_under_fire } else { pb.step_cost_near };
            }
            // Next to the opponent with a wall behind us is where a repulsor hurts most.
            if d <= 1 && k.repulsor.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false) {
                let mut p = *dest;
                let mut slam = false;
                if let Some(dir) = octant(en.pos, *dest) {
                    for _ in 0..3 {
                        if e.map.step_blocked(p, dir) {
                            slam = true;
                            break;
                        }
                        p = add(p, DIR_VEC[dir as usize]);
                    }
                }
                place -= if slam { 8.0 } else { 1.5 };
            }
            // Within two tiles a shot lands the tick it is fired: be there only with the tempo.
            if d <= 2 && k.ranged.map(|a| en.cd_attack[a as usize] <= 1).unwrap_or(false) {
                place -= if can_step_next { 2.0 } else { 6.0 };
            }
            let en_stuck = en.cd_move >= 2 && en.cd_dash >= 2;
            if d <= 2 && en_stuck && k.ranged.map(|a| my.cd_attack[a as usize] <= 1).unwrap_or(false) {
                place += 5.0;
            }
        }

        for atk in &attacks {
            if *atk == Atk::Frost {
                // An area is judged from the tile we end on: in range and in sight from there.
                let (_, range) = k.frost.unwrap();
                if chebyshev(*dest, en.pos) > range || !can_see(&e.rules, &e.map, *dest, en.pos) {
                    continue;
                }
            }
            let (mine, cost, bonus, note) = match atk {
                Atk::None => (*mv, 0.0, 0.0, goal_note),
                Atk::Ranged => (mv.with_attack(k.ranged.unwrap(), en.pos), pb.fire_cost, 0.0, "sure shot"),
                Atk::Repulsor => (mv.with_burst(k.repulsor.unwrap()), pb.repulsor_cost, 0.0, "repulsor"),
                Atk::Frost => (mv.with_attack(k.frost.unwrap().0, en.pos), pb.frost_cost, pb.frost_value, "frostburst"),
            };
            if matrix {
                cands.push((mine, place - cost + bonus, note));
                // Standing still, a shot can also lead the opponent: aim where it could step.
                if pb.lead_shots && *atk == Atk::Ranged && *kind == 0 && en.state == FState::Idle && en.cd_move == 0 {
                    for d in 0..8u8 {
                        if !e.map.step_blocked(en.pos, d) {
                            let aim = add(en.pos, DIR_VEC[d as usize]);
                            if aim != *dest {
                                cands.push((mv.with_attack(k.ranged.unwrap(), aim), place - cost, "lead shot"));
                            }
                        }
                    }
                }
                continue;
            }
            // Worst and average over the opponent's movements, with it not attacking.
            let (mut worst, mut sum) = (f64::MAX, 0.0);
            for r in &replies {
                let v = outcome(e, &s, me, &turns(mine, *r), pb, &mut sims);
                worst = worst.min(v);
                sum += v;
            }
            let avg = sum / replies.len() as f64;
            // What it could do to us this tick if it guessed the tile we end on.
            let mut threat: f64 = 0.0;
            if *kind != 2 {
                for r in &replies {
                    if en_ranged {
                        let v = outcome(e, &s, me, &turns(mine, r.with_attack(k.ranged.unwrap(), *dest)), pb, &mut sims);
                        threat = threat.min(v - worst);
                    }
                    if en_repulsor && chebyshev(*dest, en.pos) <= 2 {
                        let v = outcome(e, &s, me, &turns(mine, r.with_burst(k.repulsor.unwrap())), pb, &mut sims);
                        threat = threat.min(v - worst);
                    }
                }
            }
            let score = worst + pb.average_weight * (avg - worst) + pb.threat_weight * threat + place - cost + bonus + noise();
            if best.map(|(b, _, _)| score > b).unwrap_or(true) {
                best = Some((score, mine, note));
            }
        }
    }
    if matrix && !cands.is_empty() {
        // The opponent's options: every movement we believe it has, alone or with an
        // attack; and, standing still, a shot at each tile we could end on.
        let mut theirs: Vec<(RawTurn, f64)> = Vec::new(); // (turn, what it costs them)
        let en_frost = k.frost.filter(|(a, range)| en.state == FState::Idle && en.cd_attack[*a as usize] == 0 && dist_now <= *range);
        for r in &replies {
            let moved = if matches!(r.dash, victris_engine::turn::DirIn::Dir(_)) { pb.dash_cost } else { 0.0 };
            theirs.push((*r, moved));
            if en_ranged && pb.model_attacks {
                theirs.push((r.with_attack(k.ranged.unwrap(), my.pos), moved + pb.fire_cost));
            }
            if en_repulsor && pb.model_attacks && dist_now <= 3 {
                theirs.push((r.with_burst(k.repulsor.unwrap()), moved + pb.repulsor_cost));
            }
        }
        if en_ranged && pb.model_attacks {
            for (_, dest, kind) in &moves {
                if *kind == 1 {
                    theirs.push((RawTurn::IDLE.with_attack(k.ranged.unwrap(), *dest), pb.fire_cost));
                }
            }
        }
        if let Some((a, _)) = en_frost.filter(|_| pb.model_attacks) {
            theirs.push((RawTurn::IDLE.with_attack(a, my.pos), pb.frost_cost - pb.frost_value));
        }
        let (rows, cols) = (cands.len(), theirs.len());
        let mut payoff = vec![0.0f64; rows * cols];
        for (m, (mine, merit, _)) in cands.iter().enumerate() {
            for (c, (reply, their_cost)) in theirs.iter().enumerate() {
                payoff[m * cols + c] = cell_value(e, &s, me, &turns(*mine, *reply), pb, &mut sims) + merit + their_cost;
            }
        }
        let (mut mix, value) = solve_matrix(&payoff, rows, cols, 300);
        // Play from the mixed strategy, ignoring the tail regret matching leaves behind.
        let top = mix.iter().cloned().fold(0.0, f64::max);
        mix.iter_mut().for_each(|p| {
            if *p < 0.05_f64.min(top) {
                *p = 0.0
            }
        });
        let total: f64 = mix.iter().sum();
        let mut roll = noise() / 0.05 * total;
        let mut pick = rows - 1;
        if pb.sample {
            for (m, p) in mix.iter().enumerate() {
                if roll < *p {
                    pick = m;
                    break;
                }
                roll -= p;
            }
        } else {
            pick = (0..rows).max_by(|a, b| mix[*a].partial_cmp(&mix[*b]).unwrap()).unwrap_or(0);
        }
        return Decision { turn: cands[pick].0, sims, score: value, note: cands[pick].2 };
    }
    match best {
        Some((score, turn, note)) => Decision { turn, sims, score, note },
        None => idle,
    }
}
