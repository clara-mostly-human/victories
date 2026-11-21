//! A search led by the learned network alone (`pvnet`): no scripted play-out, no hp sums, no
//! hand-set move shares. Every line ends in the network's chance to win (or in the game's real
//! result when the line reaches one), and the network's reading of what each fighter does at a
//! position decides where the search looks first. Lines are as long as the network's
//! confidence makes them: a position where one turn is clearly right costs one visit a tick, so
//! the search reaches the positions the network rates and spends its visits finding ways into
//! them.
//!
//! Both fighters move at once. Each position keeps, per fighter, its own list of turns with
//! visit counts and results; each fighter picks by its own list (as `mcts` does), and the pair
//! of picks leads to the next position.

use crate::pvnet::{class_of, encode, turns, Inputs, Pass, PvNet};
use crate::state::State;
use crate::turn::RawTurn;
use crate::Engine;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug)]
pub struct PvConfig {
    /// how strongly the network's turn shares pull visits before results come in
    pub c: f32,
    /// share of every position's turn weights spread evenly, so no turn is shut out
    pub even: f32,
    /// the same at the root (self-play raises it to look wider)
    pub even_root: f32,
    /// a turn not yet tried counts as the position's own value less this
    pub fpu: f32,
    /// a line never runs longer than this many ticks
    pub depth: u32,
    pub max_nodes: usize,
}

impl Default for PvConfig {
    fn default() -> PvConfig {
        PvConfig { c: 1.5, even: 0.05, even_root: 0.05, fpu: 0.2, depth: 80, max_nodes: 400_000 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Act {
    pub turn: RawTurn,
    pub prior: f32,
    pub n: u32,
    /// summed results, from this fighter's side (-1 lost to 1 won)
    pub w: f32,
}

struct Node {
    acts: [Vec<Act>; 2],
    n: u32,
    /// the network's value of this position for fighter 0
    v: f32,
    kids: Vec<(u32, u32)>,
}

pub struct Search<'a> {
    e: &'a Engine,
    net: &'a PvNet,
    pub cfg: PvConfig,
    nodes: Vec<Node>,
    index: HashMap<u64, u32>,
    path: Vec<(u32, u16, u16)>,
    pass: Pass,
    x: Inputs,
    buf: Vec<RawTurn>,
    /// length of every line searched, by ticks (the last slot holds that and longer)
    pub depth_hist: [u64; 64],
}

/// A search's tree on its own, to keep between ticks (`Search::into_tree`, `Search::start_from`).
pub struct Tree {
    nodes: Vec<Node>,
    index: HashMap<u64, u32>,
}

impl Tree {
    /// Lines that ran through the root.
    pub fn visits(&self) -> u32 {
        self.nodes.first().map(|n| n.n).unwrap_or(0)
    }
}

/// A finished game's result for fighter 0.
pub fn outcome(s: &State) -> Option<f32> {
    s.result.map(|r| match r.winner {
        Some(1) => 1.0,
        Some(_) => -1.0,
        None => 0.0,
    })
}

impl<'a> Search<'a> {
    pub fn new(e: &'a Engine, net: &'a PvNet, cfg: PvConfig) -> Search<'a> {
        Search { e, net, cfg, nodes: Vec::new(), index: HashMap::new(), path: Vec::new(), pass: Pass::default(), x: Inputs::default(), buf: Vec::new(), depth_hist: [0; 64] }
    }

    /// Takes the tree out, to keep while the search itself (which borrows the engine and the
    /// network) is dropped.
    pub fn into_tree(self) -> Tree {
        Tree { nodes: self.nodes, index: self.index }
    }

    /// Continues from a kept tree. Follow with `advance` or `advance_to` to move its root to
    /// the position now on the board.
    pub fn start_from(&mut self, tree: Tree) {
        self.nodes = tree.nodes;
        self.index = tree.index;
    }

    /// Lines that ran through the root so far.
    pub fn visits(&self) -> u32 {
        self.nodes.first().map(|n| n.n).unwrap_or(0)
    }

    pub fn clear(&mut self) {
        self.nodes.clear();
        self.index.clear();
    }

    pub fn nodes(&self) -> usize {
        self.nodes.len()
    }

    fn expand(&mut self, s: &State, even: f32) -> Node {
        let mut g = [0.0f32; 2];
        let mut acts: [Vec<Act>; 2] = [Vec::new(), Vec::new()];
        for f in 0..2 {
            encode(self.e, s, f, &mut self.x);
            self.net.forward(&self.x, &mut self.pass);
            g[f] = self.pass.g;
            turns(self.e, s, f, &mut self.buf);
            let logits = &self.pass.logits;
            let top = self.buf.iter().map(|t| logits[class_of(t)]).fold(f32::MIN, f32::max);
            let mut total = 0.0f32;
            let mut list: Vec<Act> = self.buf.iter().map(|t| {
                let p = (logits[class_of(t)] - top).exp();
                total += p;
                Act { turn: *t, prior: p, n: 0, w: 0.0 }
            }).collect();
            let flat = 1.0 / list.len() as f32;
            for a in list.iter_mut() {
                a.prior = (1.0 - even) * a.prior / total.max(1e-9) + even * flat;
            }
            acts[f] = list;
        }
        Node { acts, n: 0, v: ((g[0] - g[1]) * 0.5).tanh(), kids: Vec::new() }
    }

    fn select(&self, node: usize, x: usize) -> usize {
        let nd = &self.nodes[node];
        let list = &nd.acts[x];
        if list.len() == 1 {
            return 0;
        }
        let root_n = (nd.n.max(1) as f32).sqrt();
        let unseen = if x == 0 { nd.v } else { -nd.v } - self.cfg.fpu;
        let mut best = (f32::MIN, 0);
        for (i, a) in list.iter().enumerate() {
            let q = if a.n > 0 { a.w / a.n as f32 } else { unseen };
            let u = q + self.cfg.c * a.prior * root_n / (1.0 + a.n as f32);
            if u > best.0 {
                best = (u, i);
            }
        }
        best.1
    }

    /// Runs `count` lines from `root`.
    pub fn iterate(&mut self, root: &State, count: u32) {
        if root.result.is_some() {
            return;
        }
        if self.nodes.is_empty() {
            let nd = self.expand(root, self.cfg.even_root);
            self.nodes.push(nd);
        }
        for _ in 0..count {
            let mut s = *root;
            let mut node = 0usize;
            self.path.clear();
            let value;
            loop {
                let (i0, i1) = (self.select(node, 0), self.select(node, 1));
                self.path.push((node as u32, i0 as u16, i1 as u16));
                let pair = [self.nodes[node].acts[0][i0].turn, self.nodes[node].acts[1][i1].turn];
                self.e.step(&mut s, &pair);
                if let Some(v) = outcome(&s) {
                    value = v;
                    break;
                }
                let key = (node as u64) << 32 | (i0 as u64) << 16 | i1 as u64;
                match self.index.get(&key) {
                    Some(&child) => {
                        node = child as usize;
                        if self.path.len() as u32 >= self.cfg.depth {
                            value = self.nodes[node].v;
                            break;
                        }
                    }
                    None => {
                        let child = self.expand(&s, self.cfg.even);
                        value = child.v;
                        if self.nodes.len() < self.cfg.max_nodes {
                            let id = self.nodes.len() as u32;
                            self.index.insert(key, id);
                            self.nodes[node].kids.push(((i0 as u32) << 16 | i1 as u32, id));
                            self.nodes.push(child);
                        }
                        break;
                    }
                }
            }
            self.depth_hist[self.path.len().min(63)] += 1;
            for &(n, i0, i1) in &self.path {
                let nd = &mut self.nodes[n as usize];
                nd.n += 1;
                let a = &mut nd.acts[0][i0 as usize];
                a.n += 1;
                a.w += value;
                let b = &mut nd.acts[1][i1 as usize];
                b.n += 1;
                b.w -= value;
            }
        }
    }

    /// Each fighter's turns at the root with their visits and mean result (from its own side).
    pub fn root(&self) -> [Vec<(RawTurn, u32, f32)>; 2] {
        let list = |x: usize| match self.nodes.first() {
            Some(nd) => nd.acts[x].iter().map(|a| (a.turn, a.n, if a.n > 0 { a.w / a.n as f32 } else { 0.0 })).collect(),
            None => vec![(RawTurn::IDLE, 0, 0.0)],
        };
        [list(0), list(1)]
    }

    /// The search's value of the root for fighter 0: the mean result of every line.
    pub fn value(&self) -> f32 {
        match self.nodes.first() {
            Some(nd) if nd.n > 0 => nd.acts[0].iter().map(|a| a.w).sum::<f32>() / nd.n as f32,
            Some(nd) => nd.v,
            None => 0.0,
        }
    }

    /// The network's own value of the root for fighter 0, before any search.
    pub fn net_value(&self) -> f32 {
        self.nodes.first().map(|n| n.v).unwrap_or(0.0)
    }

    /// The most visited turn of fighter `x`.
    pub fn best(&self, x: usize) -> RawTurn {
        self.nodes.first().and_then(|nd| nd.acts[x].iter().max_by_key(|a| a.n).map(|a| a.turn)).unwrap_or(RawTurn::IDLE)
    }

    /// Keeps what was searched under the position the game actually moved to, when the turns
    /// that led there are not known (a live bot sees the new position, not the opponent's
    /// turn): of the pairs of turns searched from `old_root`, the most visited one that leads
    /// to `now` becomes the root. False (and an empty search) when none does.
    pub fn advance_to(&mut self, old_root: &State, now: &State) -> bool {
        let mut best: Option<(u32, RawTurn, RawTurn)> = None;
        if let Some(nd) = self.nodes.first() {
            for &(pair, child) in &nd.kids {
                let (t0, t1) = (nd.acts[0][(pair >> 16) as usize].turn, nd.acts[1][(pair & 0xffff) as usize].turn);
                let n = self.nodes[child as usize].n;
                if best.map(|b| n > b.0).unwrap_or(true) {
                    let mut s = *old_root;
                    self.e.step(&mut s, &[t0, t1]);
                    if s == *now {
                        best = Some((n, t0, t1));
                    }
                }
            }
        }
        match best {
            Some((_, t0, t1)) => self.advance(&t0, &t1),
            None => {
                self.clear();
                false
            }
        }
    }

    /// Keeps what was searched under the pair of turns that was played: that position becomes
    /// the root. False (and an empty search) when the pair was never tried.
    pub fn advance(&mut self, t0: &RawTurn, t1: &RawTurn) -> bool {
        let found = self.nodes.first().and_then(|nd| {
            let i0 = nd.acts[0].iter().position(|a| a.turn == *t0)?;
            let i1 = nd.acts[1].iter().position(|a| a.turn == *t1)?;
            self.index.get(&((i0 as u64) << 16 | i1 as u64)).copied()
        });
        let Some(start) = found else {
            self.clear();
            return false;
        };
        let mut old = std::mem::take(&mut self.nodes);
        self.index.clear();
        // copy the kept part in the order found: each position's children follow it
        let mut order: Vec<u32> = vec![start];
        let mut at = 0;
        while at < order.len() {
            let id = order[at] as usize;
            let kids = std::mem::take(&mut old[id].kids);
            let mut renamed = Vec::with_capacity(kids.len());
            for (pair, child) in kids {
                let new_id = order.len() as u32;
                order.push(child);
                renamed.push((pair, new_id));
                self.index.insert((at as u64) << 32 | pair as u64, new_id);
            }
            old[id].kids = renamed;
            at += 1;
        }
        let mut slots: Vec<Option<Node>> = old.into_iter().map(Some).collect();
        self.nodes = order.iter().filter_map(|id| slots[*id as usize].take()).collect();
        // the new root looks as wide as a root does
        if (self.cfg.even_root - self.cfg.even).abs() > 1e-6 {
            let (even, wide) = (self.cfg.even, self.cfg.even_root);
            for x in 0..2 {
                let list = &mut self.nodes[0].acts[x];
                let flat = 1.0 / list.len() as f32;
                for a in list.iter_mut() {
                    let raw = ((a.prior - even * flat) / (1.0 - even)).max(0.0);
                    a.prior = (1.0 - wide) * raw + wide * flat;
                }
            }
        }
        true
    }
}

/// A small fast random number source for self-play and training (xorshift).
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// uniform in [0, 1)
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Picks from `visits` for self-play: the most visited turn, or with chance `wander` one drawn
/// in proportion to visits.
pub fn pick(visits: &[(RawTurn, u32)], wander: f64, rng: &mut Rng) -> RawTurn {
    let total: u64 = visits.iter().map(|v| v.1 as u64).sum();
    if total == 0 {
        return visits.first().map(|v| v.0).unwrap_or(RawTurn::IDLE);
    }
    if rng.unit() < wander {
        let mut at = (rng.unit() * total as f64) as u64;
        for (t, n) in visits {
            if at < *n as u64 {
                return *t;
            }
            at -= *n as u64;
        }
    }
    visits.iter().max_by_key(|v| v.1).map(|v| v.0).unwrap_or(RawTurn::IDLE)
}
