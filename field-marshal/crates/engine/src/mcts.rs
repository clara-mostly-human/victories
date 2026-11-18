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
use crate::movefeat::features;
use crate::observe::can_see;
use crate::rules::AttackKind;
use crate::state::{FState, State, NO_BUFF};
use crate::turn::RawTurn;
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
    /// a fighter standing still may aim its shot at a tile the opponent could step to
    pub lead: bool,
    /// the quiet play-out at a leaf also plays a wall slam that is there for the taking
    pub quiet_slam: bool,
    /// nodes fewer than this many ticks below the root order forcing turns first
    /// (0 = off: at 150 ms a move it scored 8 of 18 against 10 of 18 without, so it is
    /// off until it earns its place)
    pub forcing_depth: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config { iterations: 20_000, depth: 12, c: 1.5, quiet: 6, buff_value: 0.3, even: 0.25, value_weight: 0.0, value_gen: 0, lead: false, quiet_slam: false, forcing_depth: 0 }
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

#[derive(Default)]
struct Node {
    acts: [Vec<Act>; 2],
    n: u32,
}

/// The turns a fighter can take, with priors that sum to 1. With `forcing`, turns that
/// hurt or stun the opponent at once if it stands still are tried first, the way a chess
/// engine tries captures and checks first; this costs one engine step per turn, so it is
/// used only near the root.
pub fn actions(e: &Engine, s: &State, x: usize, even: f32, forcing: bool, lead: bool) -> Vec<Act> {
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
        move_probs(&feat)
    } else {
        let dash = yes_prob(&SPENT_WEIGHTS, &feat);
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
    let ps = if shot.is_some() { yes_prob(&SHOT_WEIGHTS, &feat) } else { 0.0 };
    let pb = if burst.is_some() { yes_prob(&BURST_WEIGHTS, &feat) } else { 0.0 };
    let pf = if frost.is_some() { yes_prob(&FROST_WEIGHTS, &feat) } else { 0.0 };
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
pub fn leaf_value(e: &Engine, s: &State, cfg: &Config) -> f64 {
    let mut st = *s;
    let mut v = 0.0;
    for _ in 0..cfg.quiet {
        if st.result.is_some() {
            break;
        }
        let turns = [quiet_turn(e, &st, 0, cfg.quiet_slam), quiet_turn(e, &st, 1, cfg.quiet_slam)];
        let before = (st.fighters[0].hp, st.fighters[1].hp);
        e.step(&mut st, &turns);
        v += reward(&st, before);
    }
    let buff = |i: usize| if st.fighters[i].power_until == NO_BUFF { 0 } else { (st.fighters[i].power_until - st.tick).max(0) };
    v += cfg.buff_value * (buff(0) - buff(1)) as f64;
    if cfg.value_weight != 0.0 && st.result.is_none() {
        v += cfg.value_weight * crate::valuefeat::value(e, &st, 0, crate::models::value_weights(cfg.value_gen)) as f64;
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
        Search { e, cfg, nodes: Vec::new(), children: HashMap::new(), total: 0.0, done: 0 }
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

    /// Empties the tree for a new root.
    pub fn start(&mut self) {
        self.nodes.clear();
        self.children.clear();
        self.nodes.push(Node::default());
        self.total = 0.0;
        self.done = 0;
    }

    /// What the tree holds at the root now.
    pub fn result(&self) -> RootResult {
        let list = |x: usize| self.nodes[0].acts[x].iter().map(|a| (a.turn, a.n, if a.n > 0 { a.w / a.n as f64 } else { 0.0 })).collect();
        RootResult { acts: [list(0), list(1)], value: self.total / self.done.max(1) as f64 }
    }

    /// Runs `count` more iterations from `root` (after `start`).
    pub fn iterate(&mut self, root: &State, fixed: [Option<RawTurn>; 2], count: u32) {
        let mut total = 0.0;
        for _ in 0..count {
            let mut s = *root;
            let mut node = 0usize;
            let mut path: Vec<(usize, usize, usize, f64)> = Vec::with_capacity(self.cfg.depth as usize);
            let mut leaf = 0.0;
            for depth in 0..self.cfg.depth {
                if self.nodes[node].acts[0].is_empty() {
                    for x in 0..2 {
                        self.nodes[node].acts[x] = match (depth, fixed[x]) {
                            (0, Some(t)) => vec![Act { turn: t, prior: 1.0, n: 0, w: 0.0 }],
                            _ => actions(self.e, &s, x, self.cfg.even, depth < self.cfg.forcing_depth, self.cfg.lead),
                        };
                    }
                    if depth > 0 {
                        leaf = leaf_value(self.e, &s, &self.cfg);
                        break;
                    }
                }
                let (i0, i1) = (self.select(node, 0), self.select(node, 1));
                let turns = [self.nodes[node].acts[0][i0].turn, self.nodes[node].acts[1][i1].turn];
                let before = (s.fighters[0].hp, s.fighters[1].hp);
                self.e.step(&mut s, &turns);
                path.push((node, i0, i1, reward(&s, before)));
                if s.result.is_some() {
                    break;
                }
                if depth + 1 == self.cfg.depth {
                    leaf = leaf_value(self.e, &s, &self.cfg);
                    break;
                }
                let key = ((node as u64) << 32) | ((i0 as u64) << 16) | i1 as u64;
                node = match self.children.get(&key) {
                    Some(c) => *c as usize,
                    None => {
                        self.nodes.push(Node::default());
                        let c = self.nodes.len() - 1;
                        self.children.insert(key, c as u32);
                        c
                    }
                };
            }
            let mut g = leaf;
            for &(n, i0, i1, r) in path.iter().rev() {
                g += r;
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
