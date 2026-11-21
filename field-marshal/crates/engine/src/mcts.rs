//! A tree search over the exact engine in which both sides look for their best move at
//! every tick (simultaneous-move Monte Carlo tree search with policy priors).
//!
//! Each node is a state; each side has its own list of turns with a prior from the fitted
//! models of the ladder's strong bots (`models`), a visit count and a value sum. One
//! iteration walks down from the root, each side choosing by value plus a prior-weighted
//! exploration term, steps the engine with the pair of turns, and stops at a node it has
//! not seen (or at the depth limit). There the position is judged by a short quiet
//! play-out (both sides only dodge and take shots that cannot miss), so that shots and
//! bursts already in the air are counted, plus a small value for a power buff still
//! running. The result is the hp taken minus the hp lost along the way (40 for a kill),
//! from fighter 0's point of view, and is added to every turn on the path.
//!
//! The priors only order the search: with enough iterations a turn the models think
//! unlikely is still found if it is good.

use crate::geom::{add, chebyshev, DIR_VEC, P};
use crate::models::{move_probs, yes_prob, BURST_WEIGHTS, FROST_WEIGHTS, SHOT_WEIGHTS, SPENT_WEIGHTS};
use crate::models_own as own;
use crate::movefeat::features;
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::map::PadKind;
use crate::turn::{DirIn, RawTurn};
use crate::Engine;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub iterations: u32,
    /// ticks searched below the root
    pub depth: u32,
    /// exploration constant
    pub c: f64,
    /// ticks of quiet play-out at a leaf
    pub quiet: u32,
    /// hp per tick of power buff still running at a leaf
    pub buff_value: f64,
    /// share of the prior that is spread evenly over all turns
    pub even: f32,
    /// weight of the fitted judgment of positions at a leaf (`models::VALUE_GENS`)
    pub value_weight: f64,
    /// which generation of the judgment (1 is the first; 0 = the latest)
    pub value_gen: usize,
    /// hp per unit of (2p - 1) of the win-probability judgment (`valuenet`) at a leaf; 0 = off
    pub net_weight: f64,
    /// The win-probability network as the whole leaf: at the end of the play-out the hp count
    /// gives way to the network's verdict read as an hp lead (`valuenet::hp_equiv`). With 1.0
    /// a leaf is worth the network's hp-equivalent minus the root's lead.
    pub net_hp: f64,
    /// Weight of the hp-to-come network (`valuenet::hp_to_come`, `VALUE_HP_NET`) at the end of
    /// the play-out: the linear judgment's role, by a network trained on the same target.
    pub net_value: f64,
    /// Cap in hp on the hp-to-come network's term (0: none), so it adds a small correction and
    /// does not jitter between sibling leaves by more than a shot is worth.
    pub net_cap: f64,
    /// Plans as moves (part 4): at the root each side may also commit to a plan for
    /// `PLAN_TICKS` ticks (go to the nearest power pad, to the nearest health pad when hurt,
    /// to the centre, at the opponent, or to a slam setup three tiles off an opponent that
    /// stands against a wall), its steps forced in the subtree below and carried
    /// on through the leaf's play-out, so the search can value goals farther than it branches.
    pub plans: bool,
    /// Plans in the play-out (part 4, second design): at a leaf each side heads for the goal that
    /// fits the position (a health pad when hurt and it gets there first, a power pad it reaches
    /// no later than the opponent, else the centre) while the quiet play-out still dodges and takes
    /// sure shots, so the leaf's judgment sees where strategic movement leads. The tree is untouched.
    pub plan_playout: bool,
    /// The win-chance leaf (part 2): with a value W > 0 the search no longer adds up hp swings.
    /// A leaf is worth W * (2p - 1), p the chance fighter 0 wins by `valuenet::win_logit` from
    /// both fighters' hp at the end of the play-out (the judgment's hp shifting them) and the
    /// time; a finished game is worth W, -W or 0. W near 48 keeps one hp about one unit at an
    /// even mid-game position.
    pub win_shape: f64,
    /// hp for each slam threat a side holds at the leaf (`valuefeat::slam_threat`), fighter 0's
    /// minus fighter 1's; and for being loaded (dash and repulsor both ready). Measured worth
    /// beyond row 13 on top-bot games: 3.5 and 1.2 hp.
    pub slam_value: f64,
    pub loaded_value: f64,
    /// The heal race at the leaf, in hp times this weight: for each health pad up or back within
    /// 60 ticks, the fighter who gets there first (path length at its move pace, or the pad's
    /// return) gains what it would heal (the pad's amount, up to its missing hp), discounted 0.97
    /// a tick. Being nearer the pad than a hurt opponent denies its heal: the leader keeps its lead.
    pub heal_value: f64,
    /// Strategy by phase: the judgment row for each phase of the game (`valuefeat::phase`:
    /// opening, late, lopsided, buffed, middle), each fitted on our own games in that phase, so
    /// the long-term goal of the phase decides what a position is worth. Each leaf is judged by its
    /// own phase; between the even game and the lopsided one the judgments blend by lead. 0 keeps `value_gen`.
    pub phase_gens: [usize; crate::valuefeat::PHASES],
    /// The strategy layer (`strategy`): each side's mode is chosen from the root position (phase,
    /// and ahead / even / behind in hp and power), and every leaf adds this weight times the
    /// difference of the two sides' goal scores (each capped at 6 hp).
    pub strategy: f64,
    /// Whose plan the leaves score: 0 or 1 for that fighter only (the opponent is left to the
    /// search's tactics, as the bot cannot know its real plan), 2 for both sides.
    pub strategy_side: u8,
    /// a fighter standing still may aim its shot at a tile the opponent could step to
    pub lead: bool,
    /// fighters (bit 0 = fighter 0, bit 1 = fighter 1) whose standing shots may lead, in
    /// addition to `lead` (both): to expect the opponent's lead shots without firing ours
    pub lead_mask: u8,
    /// the quiet play-out at a leaf also plays a wall slam that is there for the taking
    pub quiet_slam: bool,
    /// a dash may carry the repulsor from up to the dash distance farther (`actions_with`)
    pub dash_burst: bool,
    /// the priors come from the search's own choices (`models_own`) instead of the ladder bots'
    pub own_priors: bool,
    /// visits a new position gets (judged by its leaf value only) before it gets move lists
    /// and children; 0 = at once, as before
    pub expand_at: u32,
    /// the most positions the tree may hold (0 = no limit); past it, new positions are
    /// judged as leaves, which bounds the search's memory
    pub max_nodes: u32,
    /// not 0: the priors at the root are jittered from this seed, so that several searches
    /// run side by side on separate threads grow different trees (their visits are added)
    pub noise_seed: u64,
    /// nodes fewer than this many ticks below the root order forcing turns first
    /// (0 = off: at 150 ms a move it scored 8 of 18 against 10 of 18 without, so it is
    /// off until it earns its place)
    pub forcing_depth: u32,
    /// not 0: in every move list, the turns whose repulsor (standing or after a dash) slams the
    /// opponent into a wall if it holds share this much of the prior. Measured on 589 ladder
    /// games against 2400+ bots: they play such a turn on 22.8% of the ticks they have one, where
    /// the fitted move model gives those turns 3.9%.
    pub slam_prior: f32,
}

impl Default for Config {
    fn default() -> Config {
        Config { iterations: 20_000, depth: 12, c: 1.5, quiet: 6, buff_value: 0.3, even: 0.25, value_weight: 0.0, value_gen: 0, net_weight: 0.0, net_hp: 0.0, net_value: 0.0, net_cap: 0.0, plans: false, plan_playout: false, win_shape: 0.0, slam_value: 0.0, loaded_value: 0.0, heal_value: 0.0, phase_gens: [0; crate::valuefeat::PHASES], strategy: 0.0, strategy_side: 2, lead: false, lead_mask: 0, quiet_slam: false, own_priors: false, dash_burst: false, expand_at: 0, max_nodes: 0, noise_seed: 0, forcing_depth: 0, slam_prior: 0.0 }
    }
}

const SCALE: f64 = 20.0; // hp per unit of value in the exploration formula
const KILL: f64 = 40.0;

#[derive(Clone, Copy)]
pub struct Act {
    pub turn: RawTurn,
    pub prior: f32,
    pub n: u32,
    /// sum of results from this side's point of view
    pub w: f64,
}

#[derive(Default, Clone)]
struct Node {
    acts: [Vec<Act>; 2],
    n: u32,
    /// the plan each side follows here (1-based index into the search's plans; 0 for none)
    plan: [u8; 2],
    /// the forced plan ticks left from here
    left: [u8; 2],
}

/// Ticks a plan chosen at the root commits a side to (the root's tick included).
pub const PLAN_TICKS: u8 = 6;
/// Prior of each plan at the root, before the root's priors are scaled back to one.
const PLAN_PRIOR: f32 = 0.05;

/// The step a plan takes now: down its distance map, when the fighter can step (None: no step).
fn plan_step(e: &Engine, s: &State, x: usize, dist: &[u16]) -> Option<u8> {
    let f = &s.fighters[x];
    if f.state != FState::Idle || f.cd_move != 0 {
        return None;
    }
    let w = e.map.w;
    let at = |p: P| dist.get((p[1] * w + p[0]) as usize).cloned().unwrap_or(u16::MAX);
    let mut best = (at(f.pos), None);
    for dir in 0..8u8 {
        let p = add(f.pos, DIR_VEC[dir as usize]);
        if e.map.step_blocked(f.pos, dir) || !e.map.in_bounds(p) {
            continue;
        }
        if at(p) < best.0 {
            best = (at(p), Some(dir));
        }
    }
    best.1
}

/// Does `turn` move the way the plan does (its step, or no step and no dash)? Attacks are free.
fn follows(turn: &RawTurn, step: Option<u8>) -> bool {
    matches!(turn.dash, DirIn::None)
        && match step {
            Some(d) => matches!(turn.mv, DirIn::Dir(m) if m == d),
            None => matches!(turn.mv, DirIn::None),
        }
}

/// A side's turns under a plan: those of `all` that move the plan's way, or dodge the way the
/// quiet play-out would; every attack stays open. Falls back to the plan's own turn.
fn plan_acts(e: &Engine, s: &State, x: usize, dist: &[u16], slam: bool, all: Vec<Act>) -> Vec<Act> {
    let step = plan_step(e, s, x, dist);
    let q = quiet_turn(e, s, x, slam);
    let dodge = !matches!(q.mv, DirIn::None) || !matches!(q.dash, DirIn::None);
    let mut keep: Vec<Act> = all.into_iter().filter(|a| follows(&a.turn, step) || (dodge && a.turn.mv == q.mv && a.turn.dash == q.dash)).collect();
    if keep.is_empty() {
        return vec![Act { turn: plan_turn(e, s, x, dist, slam), prior: 1.0, n: 0, w: 0.0 }];
    }
    let sum: f32 = keep.iter().map(|a| a.prior).sum();
    for a in keep.iter_mut() {
        a.prior /= sum.max(1e-6);
    }
    keep.shrink_to_fit();
    keep
}

/// Steps from every tile to `target` (u16::MAX where it cannot be reached).
fn dist_map(e: &Engine, target: P) -> Vec<u16> {
    let w = e.map.w;
    let mut d = vec![u16::MAX; (w * e.map.h) as usize];
    if !e.map.in_bounds(target) || e.map.is_wall(target) {
        return d;
    }
    let idx = |p: P| (p[1] * w + p[0]) as usize;
    let mut q = std::collections::VecDeque::new();
    d[idx(target)] = 0;
    q.push_back(target);
    while let Some(p) = q.pop_front() {
        let dp = d[idx(p)];
        for dir in 0..8u8 {
            // the tile from which a step in `dir` lands on `p`
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

/// A turn that follows a plan: the quiet play-out's sure shot, slam or dodge, and otherwise
/// a step down the plan's distance map.
fn plan_turn(e: &Engine, s: &State, x: usize, dist: &[u16], slam: bool) -> RawTurn {
    let mut t = quiet_turn(e, s, x, slam);
    let f = &s.fighters[x];
    if f.state != FState::Idle || f.cd_move != 0 || !matches!(t.mv, DirIn::None) || !matches!(t.dash, DirIn::None) {
        return t;
    }
    let w = e.map.w;
    let at = |p: P| dist.get((p[1] * w + p[0]) as usize).cloned().unwrap_or(u16::MAX);
    let mut best = (at(f.pos), None);
    for dir in 0..8u8 {
        let p = add(f.pos, DIR_VEC[dir as usize]);
        if e.map.step_blocked(f.pos, dir) || !e.map.in_bounds(p) {
            continue;
        }
        if at(p) < best.0 {
            best = (at(p), Some(dir));
        }
    }
    if let Some(dir) = best.1 {
        t.mv = RawTurn::step(dir).mv;
    }
    t
}

/// The plans side `x` may commit to here: the nearest power pad (without a long buff) and,
/// when hurt, the nearest health pad, each there or back by the time it arrives; the centre;
/// the opponent's tile.
/// Kinds of plans, for reports.
pub const PLAN_NAMES: [&str; 5] = ["power pad", "health pad", "centre", "at the opponent", "slam setup"];

fn plan_targets(e: &Engine, s: &State, x: usize) -> Vec<(u8, P)> {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let mut out = Vec::new();
    for kind in [PadKind::Power, PadKind::Health] {
        if kind == PadKind::Health && f.hp >= e.rules.max_hp - 10 {
            continue;
        }
        if kind == PadKind::Power && f.power_until != NO_BUFF && f.power_until > s.tick + 10 {
            continue;
        }
        let best = e
            .map
            .pads
            .iter()
            .enumerate()
            .filter(|(i, def)| {
                let d = chebyshev(f.pos, def.pos);
                def.kind == kind && d > 0 && (s.pads[*i].available || s.pads[*i].respawn_tick - s.tick <= d + 4)
            })
            .min_by_key(|(_, def)| chebyshev(f.pos, def.pos));
        if let Some((_, def)) = best {
            out.push((if kind == PadKind::Power { 0 } else { 1 }, def.pos));
        }
    }
    if chebyshev(f.pos, e.map.center) >= 2 {
        out.push((2, e.map.center));
    }
    if o.alive && chebyshev(f.pos, o.pos) >= 3 {
        out.push((3, o.pos));
    }
    // slam setup: three tiles from the opponent on the side away from a wall it stands
    // against, from where a dash and the repulsor throw it into the wall
    if o.alive {
        let setup = (0..8u8)
            .filter(|d| e.map.step_blocked(o.pos, *d))
            .map(|d| {
                let v = DIR_VEC[d as usize];
                [o.pos[0] - 3 * v[0], o.pos[1] - 3 * v[1]]
            })
            .filter(|t| e.map.in_bounds(*t) && !e.map.is_wall(*t) && *t != f.pos)
            .min_by_key(|t| chebyshev(f.pos, *t));
        if let Some(t) = setup {
            out.push((4, t));
        }
    }
    out
}

/// The turns a fighter can take, with priors that sum to 1. With `forcing`, turns that
/// hurt or stun the opponent at once if it stands still are tried first, the way a chess
/// engine tries captures and checks first; this costs one engine step per turn, so it is
/// used only near the root.
/// `dash_burst`: a dash may also carry the repulsor when the dash can bring the fighter
/// within reach (the dash resolves first, so dash + repulsor hits from up to the dash
/// distance farther away; strong bots slam from 4 to 5 tiles this way).
pub fn actions(e: &Engine, s: &State, x: usize, even: f32, forcing: bool, lead: bool, own: bool) -> Vec<Act> {
    actions_with(e, s, x, even, forcing, lead, own, false)
}

#[allow(clippy::too_many_arguments)]
pub fn actions_with(e: &Engine, s: &State, x: usize, even: f32, forcing: bool, lead: bool, own: bool, dash_burst: bool) -> Vec<Act> {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let idle = vec![Act { turn: RawTurn::IDLE, prior: 1.0, n: 0, w: 0.0 }];
    if f.state != FState::Idle || !f.alive || !o.alive {
        return idle;
    }
    let d = chebyshev(f.pos, o.pos);
    let seen = can_see(&e.rules, &e.map, f.pos, o.pos);
    let feat = features(e, s, x);
    // movement: (turn, class) with the class's probability split among its turns
    let mut moves: Vec<(RawTurn, usize)> = vec![(RawTurn::IDLE, 0)];
    if f.cd_move == 0 {
        for dir in 0..8u8 {
            let dest = add(f.pos, DIR_VEC[dir as usize]);
            if e.map.step_blocked(f.pos, dir) || dest == o.pos {
                continue;
            }
            let after = chebyshev(dest, o.pos);
            moves.push((RawTurn::step(dir), if after < d { 1 } else if after > d { 2 } else { 3 }));
        }
    }
    if f.cd_dash == 0 {
        for dir in 0..8u8 {
            moves.push((RawTurn::dash(dir), 4));
        }
    }
    let class_p: [f32; 5] = if !seen {
        [0.3, 0.4, 0.1, 0.15, 0.05]
    } else if f.cd_move == 0 {
        if own { crate::models::move_probs_with(&own::MOVE_OWN, &feat) } else { move_probs(&feat) }
    } else {
        let dash = yes_prob(if own { &own::SPENT_OWN } else { &SPENT_WEIGHTS }, &feat);
        [1.0 - dash, 0.0, 0.0, 0.0, dash]
    };
    let mut count = [0usize; 5];
    for (_, c) in &moves {
        count[*c] += 1;
    }
    // attacks: none, shot, repulsor, frostburst, in the order the clone tries them
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
    // The repulsor after a dash, from farther away than it reaches standing.
    let burst_far = if dash_burst && burst.is_none() && f.cd_dash == 0 && d <= 3 + e.rules.dash_distance {
        e.rules.attacks.iter().enumerate().find(|(i, a)| f.cd_attack[*i] == 0 && matches!(a.kind, AttackKind::Burst { .. })).map(|(i, _)| i as u8)
    } else {
        None
    };
    let (ws, wb, wf) = if own { (&own::SHOT_OWN, &own::BURST_OWN, &own::FROST_OWN) } else { (&SHOT_WEIGHTS, &BURST_WEIGHTS, &FROST_WEIGHTS) };
    let ps = if shot.is_some() { yes_prob(ws, &feat) } else { 0.0 };
    let pb = if burst.is_some() { yes_prob(wb, &feat) } else { 0.0 };
    let pf = if frost.is_some() { yes_prob(wf, &feat) } else { 0.0 };
    let attack_p = [(1.0 - ps) * (1.0 - pb) * (1.0 - pf), ps, (1.0 - ps) * pb, (1.0 - ps) * (1.0 - pb) * pf];
    let mut out: Vec<Act> = Vec::with_capacity(moves.len() * 4);
    for (mv, c) in &moves {
        let pm = class_p[*c] / count[*c].max(1) as f32;
        out.push(Act { turn: *mv, prior: pm * attack_p[0], n: 0, w: 0.0 });
        if let Some(a) = shot {
            // Standing still, the shot can also lead an opponent that is free to step: aim
            // at a tile it could step to. Half the shot's prior goes to those aims.
            let mut leads: Vec<P> = Vec::new();
            if lead && *mv == RawTurn::IDLE && o.state == FState::Idle && o.cd_move == 0 {
                for dir in 0..8u8 {
                    let aim = add(o.pos, DIR_VEC[dir as usize]);
                    if !e.map.step_blocked(o.pos, dir) && aim != f.pos {
                        leads.push(aim);
                    }
                }
            }
            let direct = if leads.is_empty() { 1.0 } else { 0.5 };
            out.push(Act { turn: mv.with_attack(a, o.pos), prior: pm * attack_p[1] * direct, n: 0, w: 0.0 });
            for aim in &leads {
                out.push(Act { turn: mv.with_attack(a, *aim), prior: pm * attack_p[1] * 0.5 / leads.len() as f32, n: 0, w: 0.0 });
            }
        }
        if let Some(a) = burst {
            out.push(Act { turn: mv.with_burst(a), prior: pm * attack_p[2], n: 0, w: 0.0 });
        }
        if let (Some(a), 4) = (burst_far, *c) {
            // The burst model was fitted within 2 tiles; from farther it gets a fixed share.
            out.push(Act { turn: mv.with_burst(a), prior: pm * 0.3, n: 0, w: 0.0 });
        }
        if let Some(a) = frost {
            out.push(Act { turn: mv.with_attack(a, o.pos), prior: pm * attack_p[3], n: 0, w: 0.0 });
        }
    }
    let total: f32 = out.iter().map(|a| a.prior).sum();
    let flat = 1.0 / out.len() as f32;
    for a in out.iter_mut() {
        a.prior = (1.0 - even) * a.prior / total.max(1e-6) + even * flat;
    }
    if forcing {
        let gains: Vec<f32> = out
            .iter()
            .map(|a| {
                let mut st = *s;
                let turns = if x == 0 { [a.turn, RawTurn::IDLE] } else { [RawTurn::IDLE, a.turn] };
                let was = st.fighters[1 - x].state;
                let lost = e.step(&mut st, &turns).hp_lost[1 - x] as f32;
                lost + if st.fighters[1 - x].state == FState::Stunned && was != FState::Stunned { 15.0 } else { 0.0 }
            })
            .collect();
        let sum: f32 = gains.iter().sum();
        if sum > 0.0 {
            for (a, g) in out.iter_mut().zip(&gains) {
                a.prior = 0.6 * a.prior + 0.4 * g / sum;
            }
        }
    }
    // The list was sized for every move with every attack; most positions have far fewer.
    // Every position in the tree keeps two of these, so the spare room is most of its memory.
    out.shrink_to_fit();
    out
}

/// What a fighter does in the quiet play-out: the shot that cannot miss, and a step or a
/// dash out of a hit that is about to land. With `slam`, also the repulsor when it would
/// throw the opponent against a wall now. Nothing else.
pub fn quiet_turn(e: &Engine, s: &State, x: usize, slam: bool) -> RawTurn {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if f.state != FState::Idle || !o.alive {
        return RawTurn::IDLE;
    }
    let mut turn = RawTurn::IDLE;
    if slam && o.state != FState::Stunned && chebyshev(f.pos, o.pos) <= 2 {
        let burst = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Burst { .. })).filter(|i| f.cd_attack[*i] == 0);
        if let Some(a) = burst {
            let mine = RawTurn::IDLE.with_burst(a as u8);
            let mut probe = *s;
            e.step(&mut probe, &if x == 0 { [mine, RawTurn::IDLE] } else { [RawTurn::IDLE, mine] });
            if probe.fighters[1 - x].state == FState::Stunned {
                return mine;
            }
        }
    }
    if can_see(&e.rules, &e.map, f.pos, o.pos) {
        let shot = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. })).filter(|i| f.cd_attack[*i] == 0);
        if let Some(a) = shot {
            let (dx, dy) = ((f.pos[0] - o.pos[0]) as f64, (f.pos[1] - o.pos[1]) as f64);
            let flight = ((dx * dx + dy * dy).sqrt() / 2.0).ceil() as i32;
            if o.state != FState::Idle || (o.cd_move >= flight && o.cd_dash >= flight) {
                turn = turn.with_attack(a as u8, o.pos);
            }
        }
    }
    // Nothing can hurt us this tick with the opponent idle unless one of its shots or bursts
    // is out, or we stand outside the zone: skip the probes then (most ticks).
    let threatened = s.projectiles().iter().any(|p| p.owner as usize != x)
        || s.areas().iter().any(|a| a.owner as usize != x)
        || chebyshev(f.pos, e.map.center) > s.zone_radius;
    if !threatened {
        return turn;
    }
    let pair = |mine: RawTurn| if x == 0 { [mine, RawTurn::IDLE] } else { [RawTurn::IDLE, mine] };
    let mut probe = *s;
    if e.step(&mut probe, &pair(turn)).hp_lost[x] == 0 {
        return turn;
    }
    if f.cd_move == 0 {
        for dir in 0..8u8 {
            if e.map.step_blocked(f.pos, dir) {
                continue;
            }
            let mut alt = turn;
            alt.mv = RawTurn::step(dir).mv;
            let mut trial = *s;
            if e.step(&mut trial, &pair(alt)).hp_lost[x] == 0 {
                return alt;
            }
        }
    }
    if f.cd_dash == 0 {
        for dir in 0..8u8 {
            let mut alt = turn;
            alt.dash = RawTurn::dash(dir).dash;
            let mut trial = *s;
            if e.step(&mut trial, &pair(alt)).hp_lost[x] == 0 {
                return alt;
            }
        }
    }
    turn
}

/// The change in the hp difference over a tick, for fighter 0: damage dealt and taken,
/// and healing from a pad, with the kill bonus. `before` is (hp of 0, hp of 1) before it.
fn reward(s: &State, before: (i32, i32)) -> f64 {
    let mut r = ((s.fighters[0].hp.max(0) - before.0) - (s.fighters[1].hp.max(0) - before.1)) as f64;
    if !s.fighters[1].alive {
        r += KILL;
    }
    if !s.fighters[0].alive {
        r -= KILL;
    }
    r
}

/// The value of a position beyond the hp already counted, for fighter 0.
/// The goal a side heads for in a strategic play-out (`Config::plan_playout`), as an index into
/// `goals` (the pads' and the centre's distance maps), or None to stay put.
fn strategic_goal(e: &Engine, s: &State, x: usize, goals: &[(P, Vec<u16>)]) -> Option<usize> {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    if !f.alive || !o.alive {
        return None;
    }
    let w = e.map.w;
    let dist = |g: usize, p: P| goals[g].1.get((p[1] * w + p[0]) as usize).cloned().unwrap_or(u16::MAX) as i32;
    let goal_of = |pos: P| goals.iter().position(|(gp, _)| *gp == pos);
    let mut best: Option<(i32, usize)> = None;
    let hurt = f.hp <= e.rules.max_hp - 30;
    let buffed = f.power_until != NO_BUFF && f.power_until > s.tick + 10;
    for (i, def) in e.map.pads.iter().enumerate() {
        let Some(g) = goal_of(def.pos) else { continue };
        let (mine, theirs) = (dist(g, f.pos), dist(g, o.pos));
        let back = if s.pads[i].available { 0 } else { s.pads[i].respawn_tick - s.tick };
        let wanted = match def.kind {
            PadKind::Health => hurt && mine <= theirs + 1,
            PadKind::Power => !buffed && mine <= theirs,
            #[allow(unreachable_patterns)]
            _ => false,
        };
        if wanted && mine > 0 && back <= 2 * mine + 2 && best.map(|b| mine < b.0).unwrap_or(true) {
            best = Some((mine, g));
        }
    }
    if let Some((_, g)) = best {
        return Some(g);
    }
    let c = goal_of(e.map.center)?;
    if dist(c, f.pos) > 2 { Some(c) } else { None }
}

/// The heal race for fighter 0, in hp (see `Config::heal_value`); `goals` holds the pads' distance maps.
fn heal_race(e: &Engine, s: &State, goals: &[(P, Vec<u16>)]) -> f64 {
    let Some(h) = e.rules.pickups.as_ref().and_then(|p| p.health) else { return 0.0 };
    let w = e.map.w;
    let pace = e.rules.move_cooldown.max(1);
    let mut v = 0.0;
    for (i, def) in e.map.pads.iter().enumerate() {
        if def.kind != PadKind::Health {
            continue;
        }
        let Some((_, dist)) = goals.iter().find(|(p, _)| *p == def.pos) else { continue };
        let back = if s.pads[i].available { 0 } else { s.pads[i].respawn_tick - s.tick };
        if back > 60 {
            continue;
        }
        let arrive = |x: usize| -> i32 {
            let f = &s.fighters[x];
            let d = dist.get((f.pos[1] * w + f.pos[0]) as usize).cloned().unwrap_or(u16::MAX);
            if d == u16::MAX || !f.alive { i32::MAX } else { (d as i32 * pace).max(back) }
        };
        let (a0, a1) = (arrive(0), arrive(1));
        if a0 == a1 || a0.min(a1) > 60 {
            continue;
        }
        let x = if a0 < a1 { 0 } else { 1 };
        let gain = h.amount.min(e.rules.max_hp - s.fighters[x].hp.max(0)).max(0) as f64 * 0.97f64.powi(a0.min(a1));
        v += if x == 0 { gain } else { -gain };
    }
    v
}

/// A finished game's worth for fighter 0 under the win-chance leaf.
fn outcome_value(s: &State, w: f64) -> f64 {
    match s.result.and_then(|r| r.winner) {
        Some(1) => w,
        Some(_) => -w,
        None => 0.0,
    }
}

/// The win-chance leaf's worth for fighter 0: both fighters' hp, shifted by the judgment `j`
/// (hp for fighter 0), read through the win-chance curve.
fn win_shaped(e: &Engine, s: &State, j: f64, w: f64) -> f64 {
    let max = e.rules.max_hp as f64;
    let h0 = (s.fighters[0].hp.max(0) as f64 + j / 2.0).clamp(0.0, max);
    let h1 = (s.fighters[1].hp.max(0) as f64 - j / 2.0).clamp(0.0, max);
    let t = s.tick as f64 / e.rules.max_ticks.max(1) as f64;
    let z = crate::valuenet::win_logit((h0 / max) as f32, (h1 / max) as f32, t as f32) as f64;
    w * (2.0 / (1.0 + (-z).exp()) - 1.0)
}

pub fn leaf_value(e: &Engine, s: &State, cfg: &Config, plans: [Option<&[u16]>; 2], goals: &[(P, Vec<u16>)], modes: Option<[crate::strategy::Mode; 2]>) -> f64 {
    let mut st = *s;
    let mut v = 0.0;
    let strategic: [Option<usize>; 2] = if cfg.plan_playout && !goals.is_empty() { [strategic_goal(e, s, 0, goals), strategic_goal(e, s, 1, goals)] } else { [None, None] };
    let pick = |st: &State, x: usize| match (plans[x], strategic[x]) {
        (Some(d), _) => plan_turn(e, st, x, d, cfg.quiet_slam),
        (None, Some(g)) => plan_turn(e, st, x, &goals[g].1, cfg.quiet_slam),
        (None, None) => quiet_turn(e, st, x, cfg.quiet_slam),
    };
    for _ in 0..cfg.quiet {
        if st.result.is_some() {
            break;
        }
        let turns = [pick(&st, 0), pick(&st, 1)];
        let before = (st.fighters[0].hp, st.fighters[1].hp);
        e.step(&mut st, &turns);
        v += reward(&st, before);
    }
    if cfg.win_shape > 0.0 && st.result.is_some() {
        return outcome_value(&st, cfg.win_shape);
    }
    let playout = v;
    let buff = |i: usize| if st.fighters[i].power_until == NO_BUFF { 0 } else { (st.fighters[i].power_until - st.tick).max(0) };
    v += cfg.buff_value * (buff(0) - buff(1)) as f64;
    if cfg.net_weight != 0.0 && st.result.is_none() {
        // the win-probability judgment, as hp: net_weight * (2p - 1) for fighter 0
        let p = crate::valuenet::win_prob(e, &st, 0, &crate::valuenet_weights::VALUE_NET) as f64;
        v += cfg.net_weight * (2.0 * p - 1.0);
    }
    if cfg.net_hp != 0.0 && st.result.is_none() {
        let lead = (st.fighters[0].hp - st.fighters[1].hp) as f64;
        let equiv = crate::valuenet::hp_equiv(e, &st, 0, &crate::valuenet_weights::VALUE_NET, crate::valuenet_weights::VALUE_NET_HP) as f64;
        v += cfg.net_hp * (equiv - lead);
    }
    if cfg.net_value != 0.0 && st.result.is_none() {
        let t = crate::valuenet::hp_to_come(e, &st, 0, &crate::valuenet_hp_weights::VALUE_HP_NET) as f64;
        v += cfg.net_value * if cfg.net_cap > 0.0 { t.clamp(-cfg.net_cap, cfg.net_cap) } else { t };
    }
    if cfg.value_weight != 0.0 && st.result.is_none() {
        let ph = crate::valuefeat::phase(e, &st);
        let row = |p: usize| { let g = cfg.phase_gens[p]; if g > 0 { g } else { cfg.value_gen } };
        let val = |g: usize| crate::valuefeat::value(e, &st, 0, crate::models::value_weights(g)) as f64;
        let j = if cfg.phase_gens.iter().any(|g| *g > 0) && ph >= 2 {
            // between the even game and the lopsided one the goals blend over 15 to 35 hp of lead,
            // so no leaf's worth jumps where the lead crosses one line
            let lead = (st.fighters[0].hp - st.fighters[1].hp).abs() as f64;
            let a = ((lead - 15.0) / 20.0).clamp(0.0, 1.0);
            let buffed = st.fighters.iter().any(|f| f.power_until != NO_BUFF && f.power_until > st.tick);
            let even = if buffed { 3 } else { 4 };
            (1.0 - a) * val(row(even)) + a * val(row(2))
        } else {
            val(row(ph))
        };
        v += cfg.value_weight * j;
    }
    if cfg.slam_value != 0.0 && st.result.is_none() {
        v += cfg.slam_value * (crate::valuefeat::slam_threat(e, &st, 0) as i32 - crate::valuefeat::slam_threat(e, &st, 1) as i32) as f64;
    }
    if let (Some(m), true) = (modes, cfg.strategy != 0.0 && st.result.is_none()) {
        let s0 = if cfg.strategy_side != 1 { crate::strategy::score(e, &st, 0, m[0], 6.0) } else { 0.0 };
        let s1 = if cfg.strategy_side != 0 { crate::strategy::score(e, &st, 1, m[1], 6.0) } else { 0.0 };
        v += cfg.strategy * (s0 - s1);
    }
    if cfg.heal_value != 0.0 && st.result.is_none() {
        v += cfg.heal_value * heal_race(e, &st, goals);
    }
    if cfg.loaded_value != 0.0 && st.result.is_none() {
        v += cfg.loaded_value * (crate::valuefeat::loaded(e, &st, 0) as i32 - crate::valuefeat::loaded(e, &st, 1) as i32) as f64;
    }
    if cfg.win_shape > 0.0 {
        // the judgment's terms (all but the play-out's hp swing, which the final hp already holds)
        return win_shaped(e, &st, v - playout, cfg.win_shape);
    }
    v
}

pub struct Search<'a> {
    e: &'a Engine,
    pub cfg: Config,
    nodes: Vec<Node>,
    children: HashMap<u64, u32>,
    total: f64,
    done: u32,
    /// joint results at the root: (our turn index, their turn index) -> (visits, sum for fighter 0)
    pairs: HashMap<(u16, u16), (u32, f64)>,
    /// distance maps of the plans offered at the root (`Config::plans`)
    plans: Vec<Vec<u16>>,
    /// per side, the plan of each root turn (1-based; 0 for a plain turn)
    root_plan: [Vec<u8>; 2],
    /// the kind of each plan (`PLAN_NAMES`)
    plan_kind: Vec<u8>,
    /// a kept tree was loaded: offer the plans at its root on the next iteration
    plans_pending: bool,
    /// distance maps to every pad and the centre, for strategic play-outs (`Config::plan_playout`)
    goals: Vec<(P, Vec<u16>)>,
    /// every `sample_every`-th leaf position judged, kept for training the judgment on the
    /// positions the search actually judges (0: none)
    pub sample_every: u32,
    pub samples: Vec<State>,
    leaves: u32,
}

/// A search tree kept between ticks: the positions below a root, with their statistics.
/// Nodes hold no states (each iteration replays its path from the root), so a subtree can
/// be carried to the next tick when the root it hangs from is the position reached.
pub struct Tree {
    nodes: Vec<Node>,
    children: HashMap<u64, u32>,
}

impl Tree {
    /// Visits at the root.
    pub fn visits(&self) -> u32 {
        self.nodes.first().map(|n| n.n).unwrap_or(0)
    }
}

/// What the search found at the root, for each side: (turn, visits, average result in hp
/// from that side's point of view).
pub struct RootResult {
    pub acts: [Vec<(RawTurn, u32, f64)>; 2],
    /// average result of all iterations, for fighter 0
    pub value: f64,
}

impl<'a> Search<'a> {
    pub fn new(e: &'a Engine, cfg: Config) -> Search<'a> {
        Search { e, cfg, nodes: Vec::new(), children: HashMap::new(), total: 0.0, done: 0, pairs: HashMap::new(), plans: Vec::new(), root_plan: [Vec::new(), Vec::new()], plan_kind: Vec::new(), plans_pending: false, goals: if cfg.plan_playout || cfg.heal_value != 0.0 { e.map.pads.iter().map(|p| p.pos).chain(std::iter::once(e.map.center)).map(|p| (p, dist_map(e, p))).collect() } else { Vec::new() }, sample_every: 0, samples: Vec::new(), leaves: 0 }
    }

    fn select(&self, node: usize, side: usize) -> usize {
        let nd = &self.nodes[node];
        let root_n = ((nd.n + 1) as f64).sqrt();
        let mut best = (f64::MIN, 0);
        for (i, a) in nd.acts[side].iter().enumerate() {
            let q = if a.n > 0 { a.w / a.n as f64 / SCALE } else { 0.0 };
            let u = self.cfg.c * a.prior as f64 * root_n / (1.0 + a.n as f64);
            if q + u > best.0 {
                best = (q + u, i);
            }
        }
        best.1
    }

    /// Searches from `root`. A side with a turn in `fixed` plays that turn at the root.
    pub fn run(&mut self, root: &State, fixed: [Option<RawTurn>; 2]) -> RootResult {
        self.start();
        self.iterate(root, fixed, self.cfg.iterations);
        self.result()
    }

    /// Starts from a tree kept from the previous tick (see `reroot`).
    pub fn start_from(&mut self, tree: Tree) {
        self.start();
        if !tree.nodes.is_empty() {
            self.nodes = tree.nodes;
            self.children = tree.children;
            // plans are offered afresh at every root (`reroot` keeps no positions under old plans)
            self.plans_pending = self.cfg.plans;
        }
    }

    /// The subtree below the root that the game actually went into: fighter `me` played
    /// `ours` and the opponent played whichever of its root turns, stepped from `old_root`,
    /// gives `new_root` (fighters' tiles, hp, states and cooldowns, and the tick). `None`
    /// when no root turn of the opponent explains the new position (for instance a guess
    /// under fog that turned out wrong), or when that pair was never searched.
    pub fn reroot(&self, old_root: &State, me: usize, ours: &RawTurn, new_root: &State) -> Option<Tree> {
        let root = self.nodes.first()?;
        let i_me = root.acts[me].iter().position(|a| a.turn == *ours)?;
        let same = |a: &State, b: &State| {
            a.tick == b.tick
                && (0..2).all(|x| {
                    let (f, g) = (&a.fighters[x], &b.fighters[x]);
                    f.pos == g.pos && f.hp == g.hp && f.state == g.state && f.cd_move == g.cd_move && f.cd_dash == g.cd_dash && f.cd_attack == g.cd_attack
                })
        };
        for (i_them, a) in root.acts[1 - me].iter().enumerate() {
            let mut turns = [RawTurn::IDLE; 2];
            turns[me] = *ours;
            turns[1 - me] = a.turn;
            let mut st = *old_root;
            self.e.step(&mut st, &turns);
            if !same(&st, new_root) {
                continue;
            }
            let (i0, i1) = if me == 0 { (i_me, i_them) } else { (i_them, i_me) };
            let key = ((0u64) << 32) | ((i0 as u64) << 16) | i1 as u64;
            let start = *self.children.get(&key)? as usize;
            // positions under a plan hang on plans of this search: not carried over
            if self.nodes[start].plan != [0, 0] {
                return None;
            }
            // Copy the subtree, numbering its nodes afresh.
            let mut map: HashMap<u32, u32> = HashMap::new();
            let mut nodes: Vec<Node> = Vec::new();
            let mut order = vec![start as u32];
            map.insert(start as u32, 0);
            nodes.push(self.nodes[start].clone());
            let mut k = 0;
            let mut by_parent: HashMap<u32, Vec<(u64, u32)>> = HashMap::new();
            for (key, c) in self.children.iter() {
                by_parent.entry((key >> 32) as u32).or_default().push((*key & 0xffff_ffff, *c));
            }
            let mut children: HashMap<u64, u32> = HashMap::new();
            while k < order.len() {
                let old = order[k];
                let new = map[&old];
                if let Some(list) = by_parent.get(&old) {
                    for (low, c) in list {
                        if self.nodes[*c as usize].plan != [0, 0] {
                            continue;
                        }
                        let nc = *map.entry(*c).or_insert_with(|| {
                            nodes.push(self.nodes[*c as usize].clone());
                            order.push(*c);
                            (nodes.len() - 1) as u32
                        });
                        children.insert(((new as u64) << 32) | low, nc);
                    }
                }
                k += 1;
            }
            return Some(Tree { nodes, children });
        }
        None
    }

    /// The tree, to keep until the next tick.
    pub fn into_tree(self) -> Tree {
        Tree { nodes: self.nodes, children: self.children }
    }

    /// Empties the tree for a new root.
    pub fn start(&mut self) {
        self.nodes.clear();
        self.children.clear();
        self.pairs.clear();
        self.plans.clear();
        self.plan_kind.clear();
        self.plans_pending = false;
        self.root_plan = [Vec::new(), Vec::new()];
        self.nodes.push(Node::default());
        self.total = 0.0;
        self.done = 0;
    }

    /// The root as a matrix game: the `k` most visited turns of each side, each cell the
    /// average result of that pair of turns (or, with fewer than 3 visits, the mean of the
    /// two turns' own averages), solved by regret matching. Returns fighter `me`'s turn
    /// with the most weight in the solution. Decoupled visit counts follow each side's
    /// exploration; the solve answers what the opponent's best mixture against us is.
    pub fn root_solve(&self, me: usize, k: usize, iters: u32) -> Option<RawTurn> {
        let root = self.nodes.first()?;
        let top = |x: usize| -> Vec<usize> {
            let mut idx: Vec<usize> = (0..root.acts[x].len()).collect();
            idx.sort_by(|a, b| root.acts[x][*b].n.cmp(&root.acts[x][*a].n));
            idx.truncate(k.max(1));
            idx
        };
        let (rows, cols) = (top(0), top(1));
        if rows.is_empty() || cols.is_empty() {
            return None;
        }
        let mean = |x: usize, i: usize| {
            let a = &root.acts[x][i];
            let q = if a.n > 0 { a.w / a.n as f64 } else { 0.0 };
            if x == 0 { q } else { -q }
        };
        let (nr, nc) = (rows.len(), cols.len());
        let mut m = vec![0.0f64; nr * nc];
        for (r, &i0) in rows.iter().enumerate() {
            for (c, &i1) in cols.iter().enumerate() {
                m[r * nc + c] = match self.pairs.get(&(i0 as u16, i1 as u16)) {
                    Some((n, w)) if *n >= 3 => w / *n as f64,
                    _ => 0.5 * (mean(0, i0) + mean(1, i1)),
                };
            }
        }
        // Regret matching: fighter 0 maximises, fighter 1 minimises.
        let (mut reg_r, mut reg_c) = (vec![0.0f64; nr], vec![0.0f64; nc]);
        let (mut avg_r, mut avg_c) = (vec![0.0f64; nr], vec![0.0f64; nc]);
        let strat = |reg: &Vec<f64>| -> Vec<f64> {
            let pos: f64 = reg.iter().map(|v| v.max(0.0)).sum();
            if pos > 0.0 { reg.iter().map(|v| v.max(0.0) / pos).collect() } else { vec![1.0 / reg.len() as f64; reg.len()] }
        };
        for _ in 0..iters {
            let (sr, sc) = (strat(&reg_r), strat(&reg_c));
            let row_vals: Vec<f64> = (0..nr).map(|r| (0..nc).map(|c| sc[c] * m[r * nc + c]).sum()).collect();
            let col_vals: Vec<f64> = (0..nc).map(|c| (0..nr).map(|r| sr[r] * m[r * nc + c]).sum()).collect();
            let vr: f64 = (0..nr).map(|r| sr[r] * row_vals[r]).sum();
            for r in 0..nr {
                reg_r[r] += row_vals[r] - vr;
                avg_r[r] += sr[r];
            }
            for c in 0..nc {
                reg_c[c] += vr - col_vals[c];
                avg_c[c] += sc[c];
            }
        }
        let pick = if me == 0 {
            rows[(0..nr).max_by(|a, b| avg_r[*a].partial_cmp(&avg_r[*b]).unwrap()).unwrap()]
        } else {
            cols[(0..nc).max_by(|a, b| avg_c[*a].partial_cmp(&avg_c[*b]).unwrap()).unwrap()]
        };
        Some(root.acts[me][pick].turn)
    }

    /// Positions in the tree.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Bytes held by the move lists (capacity), the bulk of the tree's memory.
    pub fn list_bytes(&self) -> usize {
        self.nodes.iter().map(|n| (n.acts[0].capacity() + n.acts[1].capacity()) * std::mem::size_of::<Act>()).sum::<usize>()
            + self.nodes.capacity() * std::mem::size_of::<Node>()
    }

    /// Adds the plans each side may commit to at the root (`Config::plans`) as root turns.
    fn add_root_plans(&mut self, s: &State, fixed: [Option<RawTurn>; 2]) {
        for x in 0..2 {
            self.root_plan[x] = vec![0; self.nodes[0].acts[x].len()];
            if fixed[x].is_some() {
                continue;
            }
            let plain = self.nodes[0].acts[x].len();
            for (kind, target) in plan_targets(self.e, s, x) {
                let dist = dist_map(self.e, target);
                // the plan's root turns: copies of the plain turns that move its way (each with its
                // attack), sharing the plan's prior in proportion to their own
                let step = plan_step(self.e, s, x, &dist);
                let mut copies: Vec<Act> = self.nodes[0].acts[x][..plain].iter().filter(|a| follows(&a.turn, step)).map(|a| Act { turn: a.turn, prior: a.prior, n: 0, w: 0.0 }).collect();
                if copies.is_empty() {
                    copies.push(Act { turn: plan_turn(self.e, s, x, &dist, self.cfg.quiet_slam), prior: 1.0, n: 0, w: 0.0 });
                }
                let sum: f32 = copies.iter().map(|a| a.prior).sum();
                self.plans.push(dist);
                self.plan_kind.push(kind);
                let pid = self.plans.len() as u8;
                for mut a in copies {
                    a.prior = PLAN_PRIOR * a.prior / sum.max(1e-6);
                    self.nodes[0].acts[x].push(a);
                    self.root_plan[x].push(pid);
                }
            }
            let sum: f32 = self.nodes[0].acts[x].iter().map(|a| a.prior).sum();
            for a in self.nodes[0].acts[x].iter_mut() {
                a.prior /= sum.max(1e-6);
            }
        }
    }

    /// The root's turns that are plans, per side: (kind, visits, average result for that side).
    pub fn plan_stats(&self) -> [Vec<(u8, u32, f64)>; 2] {
        let mut out = [Vec::new(), Vec::new()];
        let Some(root) = self.nodes.first() else { return out };
        for x in 0..2 {
            for (i, a) in root.acts[x].iter().enumerate() {
                let pid = self.root_plan[x].get(i).cloned().unwrap_or(0);
                if pid != 0 {
                    out[x].push((self.plan_kind[pid as usize - 1], a.n, if a.n > 0 { a.w / a.n as f64 } else { 0.0 }));
                }
            }
        }
        out
    }

    /// The distance maps of the plans in `ids` (1-based; 0 for none).
    fn dists(&self, ids: [u8; 2]) -> [Option<&[u16]>; 2] {
        let get = |id: u8| if id == 0 { None } else { self.plans.get(id as usize - 1).map(|v| v.as_slice()) };
        [get(ids[0]), get(ids[1])]
    }

    /// What the tree holds at the root now.
    pub fn result(&self) -> RootResult {
        // Plans add root turns that repeat a plain turn: the same turn's visits and results are
        // summed, so a good turn is not outvoted because its visits were split between copies.
        let list = |x: usize| {
            let mut out: Vec<(RawTurn, u32, f64)> = Vec::new();
            for a in self.nodes[0].acts[x].iter() {
                match out.iter_mut().find(|(t, _, _)| *t == a.turn) {
                    Some(e) => {
                        e.1 += a.n;
                        e.2 += a.w;
                    }
                    None => out.push((a.turn, a.n, a.w)),
                }
            }
            for e in out.iter_mut() {
                e.2 = if e.1 > 0 { e.2 / e.1 as f64 } else { 0.0 };
            }
            out
        };
        RootResult { acts: [list(0), list(1)], value: self.total / self.done.max(1) as f64 }
    }

    /// Runs `count` more iterations from `root` (after `start`).
    pub fn iterate(&mut self, root: &State, fixed: [Option<RawTurn>; 2], count: u32) {
        // Strategy by phase: each leaf is judged by its own phase's judgment (taking the phase from
        // the root for the whole search lost 12-23-1 against the flagship).
        let leaf_cfg = self.cfg;
        // the strategy layer: both sides' modes from the position at the root
        let modes = if self.cfg.strategy != 0.0 { Some([crate::strategy::choose(self.e, root, 0), crate::strategy::choose(self.e, root, 1)]) } else { None };
        let mut total = 0.0;
        for _ in 0..count {
            let mut s = *root;
            let mut node = 0usize;
            let mut path: Vec<(usize, usize, usize, f64)> = Vec::with_capacity(self.cfg.depth as usize);
            let mut leaf = 0.0;
            // the plans the two sides took at the root in this iteration, carried into the play-out
            let mut took = [0u8; 2];
            for depth in 0..self.cfg.depth {
                if self.nodes[node].acts[0].is_empty() {
                    // With `expand_at`, a new position is first only judged; it gets its
                    // move lists (the costly part, in time and memory) once visited again.
                    if depth > 0 && self.nodes[node].n < self.cfg.expand_at {
                        self.nodes[node].n += 1;
                        leaf = leaf_value(self.e, &s, &leaf_cfg, self.dists(took), &self.goals, modes);
                        break;
                    }
                    for x in 0..2 {
                        let pid = self.nodes[node].plan[x];
                        self.nodes[node].acts[x] = match (depth, fixed[x]) {
                            (0, Some(t)) => vec![Act { turn: t, prior: 1.0, n: 0, w: 0.0 }],
                            _ if pid != 0 => plan_acts(
                                self.e,
                                &s,
                                x,
                                &self.plans[pid as usize - 1],
                                self.cfg.quiet_slam,
                                actions_with(self.e, &s, x, self.cfg.even, false, self.cfg.lead || (self.cfg.lead_mask >> x) & 1 == 1, self.cfg.own_priors, self.cfg.dash_burst),
                            ),
                            _ => actions_with(self.e, &s, x, self.cfg.even, depth < self.cfg.forcing_depth, self.cfg.lead || (self.cfg.lead_mask >> x) & 1 == 1, self.cfg.own_priors, self.cfg.dash_burst),
                        };
                    }
                    if self.cfg.slam_prior > 0.0 {
                        for x in 0..2 {
                            slam_reweight(self.e, &s, x, &mut self.nodes[node].acts[x], self.cfg.slam_prior);
                        }
                    }
                    if depth == 0 && self.cfg.plans {
                        self.add_root_plans(&s, fixed);
                        self.plans_pending = false;
                    }
                    if depth > 0 && self.cfg.expand_at == 0 {
                        leaf = leaf_value(self.e, &s, &leaf_cfg, self.dists(took), &self.goals, modes);
                        break;
                    }
                    if depth == 0 && self.cfg.noise_seed != 0 {
                        let mut rng = self.cfg.noise_seed | 1;
                        for x in 0..2 {
                            let acts = &mut self.nodes[node].acts[x];
                            for a in acts.iter_mut() {
                                a.prior *= 0.5 + crate::clone::next(&mut rng);
                            }
                            let sum: f32 = acts.iter().map(|a| a.prior).sum();
                            for a in acts.iter_mut() {
                                a.prior /= sum.max(1e-6);
                            }
                        }
                    }
                }
                if depth == 0 && self.plans_pending {
                    self.add_root_plans(&s, fixed);
                    self.plans_pending = false;
                }
                let (i0, i1) = (self.select(node, 0), self.select(node, 1));
                if node == 0 && self.cfg.plans {
                    took = [self.root_plan[0].get(i0).cloned().unwrap_or(0), self.root_plan[1].get(i1).cloned().unwrap_or(0)];
                }
                let turns = [self.nodes[node].acts[0][i0].turn, self.nodes[node].acts[1][i1].turn];
                let before = (s.fighters[0].hp, s.fighters[1].hp);
                self.e.step(&mut s, &turns);
                let r = if self.cfg.win_shape > 0.0 { 0.0 } else { reward(&s, before) };
                path.push((node, i0, i1, r));
                if s.result.is_some() {
                    if self.cfg.win_shape > 0.0 {
                        leaf = outcome_value(&s, self.cfg.win_shape);
                    }
                    break;
                }
                if depth + 1 == self.cfg.depth {
                    leaf = leaf_value(self.e, &s, &leaf_cfg, self.dists(took), &self.goals, modes);
                    break;
                }
                let key = ((node as u64) << 32) | ((i0 as u64) << 16) | i1 as u64;
                node = match self.children.get(&key) {
                    Some(c) => *c as usize,
                    None if self.cfg.max_nodes > 0 && self.nodes.len() >= self.cfg.max_nodes as usize => {
                        // The tree is as big as it may grow: judge this position as a leaf.
                        leaf = leaf_value(self.e, &s, &leaf_cfg, self.dists(took), &self.goals, modes);
                        break;
                    }
                    None => {
                        let mut child = Node::default();
                        if self.cfg.plans {
                            // a plan taken at the root, or one still running here, goes on below
                            for x in 0..2 {
                                let (pid, left) = if node == 0 {
                                    (self.root_plan[x].get(if x == 0 { i0 } else { i1 }).cloned().unwrap_or(0), PLAN_TICKS)
                                } else {
                                    (self.nodes[node].plan[x], self.nodes[node].left[x])
                                };
                                if pid != 0 && left > 1 {
                                    child.plan[x] = pid;
                                    child.left[x] = left - 1;
                                }
                            }
                        }
                        self.nodes.push(child);
                        let c = self.nodes.len() - 1;
                        self.children.insert(key, c as u32);
                        c
                    }
                };
            }
            if self.sample_every > 0 && s.result.is_none() {
                self.leaves += 1;
                if self.leaves % self.sample_every == 0 {
                    self.samples.push(s);
                }
            }
            let mut g = leaf;
            for &(n, i0, i1, r) in path.iter().rev() {
                g += r;
                if n == 0 {
                    let p = self.pairs.entry((i0 as u16, i1 as u16)).or_insert((0, 0.0));
                    p.0 += 1;
                    p.1 += g;
                }
                let nd = &mut self.nodes[n];
                nd.n += 1;
                nd.acts[0][i0].n += 1;
                nd.acts[0][i0].w += g;
                nd.acts[1][i1].n += 1;
                nd.acts[1][i1].w -= g;
            }
            total += g;
        }
        self.total += total;
        self.done += count;
    }
}

/// `Config::slam_prior`: gives the turns of `acts` whose repulsor slams the opponent into a wall
/// if it holds the share `target` of the prior (only raises it; needs both kinds of turn).
fn slam_reweight(e: &Engine, s: &State, x: usize, acts: &mut [Act], target: f32) {
    if acts.len() < 2 || s.fighters[x].state != FState::Idle {
        return;
    }
    let is_burst = |t: &RawTurn| matches!(t.attack, crate::turn::AttackIn::Named { attack, .. } if matches!(e.rules.attacks[attack as usize].kind, AttackKind::Burst { .. }));
    let slams: Vec<bool> = acts
        .iter()
        .map(|a| {
            if !is_burst(&a.turn) {
                return false;
            }
            let mut st = *s;
            let mut turns = [RawTurn::IDLE; 2];
            turns[x] = a.turn;
            e.step(&mut st, &turns).slammed[1 - x]
        })
        .collect();
    let m: f32 = acts.iter().zip(&slams).filter(|(_, &b)| b).map(|(a, _)| a.prior).sum();
    let total: f32 = acts.iter().map(|a| a.prior).sum();
    if m <= 0.0 || m >= total || m >= target * total {
        return;
    }
    let (up, down) = (target * total / m, (1.0 - target) * total / (total - m));
    for (a, &b) in acts.iter_mut().zip(&slams) {
        a.prior *= if b { up } else { down };
    }
}
