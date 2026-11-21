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

/// `Search::root_cells`: the root as a table, `rows` turns of one fighter by `cols` of the
/// other, row by row; a cell is (mean result for the rows' fighter, lines through the pair).
pub struct RootCells {
    pub rows: usize,
    pub cols: usize,
    pub cells: Vec<(f32, u32)>,
}

/// One of our turns as `Search::weigh` left it.
#[derive(Clone, Copy, Debug)]
pub struct WeighedTurn {
    pub turn: RawTurn,
    /// its result against the mix it was chosen by: `trust` of the turns the search expects of
    /// the other fighter and the rest their best answer to our usual play
    pub worth: f32,
    /// its mean against the turns the search expects of the other fighter
    pub expected: f32,
    /// its result against their best answer to our usual play, and that answer (one turn of
    /// theirs, the same for every turn weighed)
    pub worst: f32,
    pub worst_to: RawTurn,
    /// lines under the turn
    pub lines: u32,
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
            self.line(root, None);
        }
    }

    /// Searching while waiting for the next tick: `count` lines from `root` that all begin
    /// with the turn fighter `me` has already sent, so the work lands under the positions the
    /// game can now move to and `advance_to` finds it there. The opponent's first turn is
    /// spread over its turns in proportion to the visits they already have (not by what does
    /// best against our turn, which it cannot know). False when `ours` is not one of the
    /// root's turns or nothing was searched from `root` yet: nothing is done then.
    pub fn ponder(&mut self, root: &State, me: usize, ours: &RawTurn, count: u32) -> bool {
        if root.result.is_some() || self.nodes.is_empty() {
            return false;
        }
        let Some(mine) = self.nodes[0].acts[me].iter().position(|a| a.turn == *ours) else { return false };
        let theirs: Vec<f64> = self.nodes[0].acts[1 - me].iter().map(|a| a.n as f64).collect();
        let total: f64 = theirs.iter().sum();
        if total <= 0.0 {
            return false;
        }
        let mut given = vec![0u32; theirs.len()];
        for j in 0..count {
            // the opponent turn furthest behind its share
            let mut best = (f64::MIN, 0usize);
            for (i, n) in theirs.iter().enumerate() {
                let behind = n / total * (j + 1) as f64 - given[i] as f64;
                if *n > 0.0 && behind > best.0 {
                    best = (behind, i);
                }
            }
            given[best.1] += 1;
            let first = if me == 0 { (mine, best.1) } else { (best.1, mine) };
            self.line(root, Some(first));
        }
        true
    }

    /// One line from `root` to a new position or the end of the game; `first` fixes both
    /// fighters' turns at the root. Returns the line's result for fighter 0.
    fn line(&mut self, root: &State, first: Option<(usize, usize)>) -> f32 {
        let mut s = *root;
        let mut node = 0usize;
        self.path.clear();
        let value;
        loop {
            let (i0, i1) = match first {
                Some(pair) if node == 0 => pair,
                _ => (self.select(node, 0), self.select(node, 1)),
            };
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
        for k in 0..self.path.len() {
            let (n, i0, i1) = self.path[k];
            let nd = &mut self.nodes[n as usize];
            nd.n += 1;
            let a = &mut nd.acts[0][i0 as usize];
            a.n += 1;
            a.w += value;
            let b = &mut nd.acts[1][i1 as usize];
            b.n += 1;
            b.w -= value;
        }
        value
    }

    /// Each fighter's turns at the root with their visits and mean result (from its own side).
    pub fn root(&self) -> [Vec<(RawTurn, u32, f32)>; 2] {
        let list = |x: usize| match self.nodes.first() {
            Some(nd) => nd.acts[x].iter().map(|a| (a.turn, a.n, if a.n > 0 { a.w / a.n as f32 } else { 0.0 })).collect(),
            None => vec![(RawTurn::IDLE, 0, 0.0)],
        };
        [list(0), list(1)]
    }

    /// Each fighter's turns at the root with the visits each one earned: its visits less the
    /// ones it only got because the root was made to look wider than the search looks
    /// anywhere else (`even_root` above `even`, as self-play asks). A turn other than the most
    /// visited keeps at most the visits that bring its upper bound, counted with its ordinary
    /// share, level with the most visited turn's. What is left is what the search found, not
    /// what it was told to try: a network taught the plain visits learns the extra width as if
    /// it were a finding, and its shares stay blurred however many generations it trains.
    pub fn root_earned(&self) -> [Vec<(RawTurn, f32)>; 2] {
        let list = |x: usize| -> Vec<(RawTurn, f32)> {
            let Some(nd) = self.nodes.first() else {
                return vec![(RawTurn::IDLE, 0.0)];
            };
            let acts = &nd.acts[x];
            let (even, wide) = (self.cfg.even, self.cfg.even_root);
            let flat = 1.0 / acts.len().max(1) as f32;
            // the share this turn would have anywhere but at the root
            let share = |a: &Act| (1.0 - even) * ((a.prior - wide * flat) / (1.0 - wide).max(1e-6)).max(0.0) + even * flat;
            let root_n = (nd.n.max(1) as f32).sqrt();
            let top = (0..acts.len()).max_by_key(|i| acts[*i].n).unwrap_or(0);
            if acts.is_empty() || acts[top].n == 0 {
                return acts.iter().map(|a| (a.turn, 0.0)).collect();
            }
            let bound = acts[top].w / acts[top].n as f32 + self.cfg.c * share(&acts[top]) * root_n / (1.0 + acts[top].n as f32);
            acts.iter()
                .enumerate()
                .map(|(i, a)| {
                    if i == top || a.n == 0 {
                        return (a.turn, a.n as f32);
                    }
                    let gap = bound - a.w / a.n as f32;
                    let earned = if gap > 1e-6 { (self.cfg.c * share(a) * root_n / gap - 1.0).clamp(0.0, a.n as f32) } else { a.n as f32 };
                    (a.turn, earned)
                })
                .collect()
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

    /// Every position in the tree, for measuring: (ticks below the root, lines through it, the
    /// position). The same position reached by two orders of turns is in the tree twice, and
    /// is listed twice.
    pub fn positions(&self, root: &State) -> Vec<(u32, u32, State)> {
        let mut out = Vec::with_capacity(self.nodes.len());
        if self.nodes.is_empty() {
            return out;
        }
        let mut stack: Vec<(usize, u32, State)> = vec![(0, 0, *root)];
        while let Some((node, depth, s)) = stack.pop() {
            let nd = &self.nodes[node];
            out.push((depth, nd.n, s));
            for &(pair, child) in &nd.kids {
                let mut c = s;
                self.e.step(&mut c, &[nd.acts[0][(pair >> 16) as usize].turn, nd.acts[1][(pair & 0xffff) as usize].turn]);
                stack.push((child as usize, depth + 1, c));
            }
        }
        out
    }

    /// The root as a table for fighter `x`: its turns down (in the order of `root()`), the
    /// other fighter's across. A cell holds the mean result for `x` of every line that began
    /// with that pair of turns, and how many lines that was (0: the pair was never tried).
    pub fn root_cells(&self, x: usize) -> RootCells {
        let Some(nd) = self.nodes.first() else {
            return RootCells { rows: 0, cols: 0, cells: Vec::new() };
        };
        let (rows, cols) = (nd.acts[x].len(), nd.acts[1 - x].len());
        let side = if x == 0 { 1.0 } else { -1.0 };
        let mut t = RootCells { rows, cols, cells: vec![(0.0, 0); rows * cols] };
        for &(pair, child) in &nd.kids {
            let (i0, i1) = ((pair >> 16) as usize, (pair & 0xffff) as usize);
            let (i, j) = if x == 0 { (i0, i1) } else { (i1, i0) };
            let c = &self.nodes[child as usize];
            // the line that made the position brought the network's value of it; every line
            // since is in the position's own counts
            let sum: f32 = c.v + c.acts[0].iter().map(|a| a.w).sum::<f32>();
            t.cells[i * cols + j] = (side * sum / (1 + c.n) as f32, 1 + c.n);
        }
        t
    }

    /// One cell of `root_cells`: fighter `x`'s turn `i` against the other fighter's turn `j`.
    fn cell(&self, x: usize, i: usize, j: usize) -> (f32, u32) {
        let (i0, i1) = if x == 0 { (i, j) } else { (j, i) };
        match self.index.get(&((i0 as u64) << 16 | i1 as u64)) {
            Some(&child) => {
                let c = &self.nodes[child as usize];
                let sum: f32 = c.v + c.acts[0].iter().map(|a| a.w).sum::<f32>();
                (if x == 0 { sum } else { -sum } / (1 + c.n) as f32, 1 + c.n)
            }
            None => (0.0, 0),
        }
    }

    /// Chooses fighter `x`'s turn at the root as one of two choices made at the same moment.
    ///
    /// `iterate` scores each fighter's turns on their own: a turn's mean is over the lines
    /// that began with it, whatever the other fighter was trying on those lines. The two
    /// sides move together as the search goes (while we try standing still they settle on the
    /// answer to standing still; by the time they try it often we have moved on to the answer
    /// to that), so a turn of ours can keep a good mean although it does badly against a turn
    /// the search itself expects from them, and a turn of theirs that would wreck our most
    /// visited turn can end with next to no visits. The search's value of the position is
    /// sound; what it cannot give is one turn to play out of a position where the right play
    /// depends on theirs.
    ///
    /// Here the root is taken as the table it is: our turns down, theirs across, a cell the
    /// mean of the lines that began with that pair. The turns of ours the search took
    /// seriously (2% of the visits or more, the eight most visited at most) are compared by
    /// their result against one mix of the other fighter's turns:
    ///  - `trust` of it is what the search expects of them: the shares of its visits;
    ///  - the rest (none at trust 1, the setting that tested best) is their one best answer
    ///    to our usual play. They cannot see our turn, but
    ///    they can know what we usually do: the answer is the turn of theirs that does best
    ///    against our turns in the shares the search gave them, found by a search of its own
    ///    (a third of `lines`; the rule the search uses everywhere: the result so far plus
    ///    the network's share for a turn little tried). It is one turn for the whole table,
    ///    not a separate perfect counter to each turn of ours: against a counter to
    ///    everything nothing is worth doing, against the one punishment of our usual play the
    ///    best turn is whatever does best when it comes.
    /// The lines go where they settle the choice: every turn in the running gets the same
    /// number, each line through the pair whose few lines weigh most on the turn's result,
    /// then the worse half of the turns drops out and the rest share the next round (so the
    /// last two are compared on the most lines). The most visited turn, the search's own
    /// choice, stays in the running to the end, and another turn takes its place only when
    /// it is better by three standard errors of the difference (from the spread of the lines
    /// run here and the lines behind each pair): where the right turn hangs on theirs the
    /// gap is many times that, and a gap inside the noise is no reason to leave the turn the
    /// whole search stands behind. Returns the turns, the one to play first.
    pub fn weigh(&mut self, root: &State, x: usize, lines: u32, trust: f32) -> Vec<WeighedTurn> {
        let plain = |s: &Search, n: u32| {
            let v = if x == 0 { s.value() } else { -s.value() };
            vec![WeighedTurn { turn: s.best(x), worth: v, expected: v, worst: v, worst_to: RawTurn::IDLE, lines: n }]
        };
        if self.nodes.is_empty() || root.result.is_some() {
            return plain(self, 0);
        }
        let nd = &self.nodes[0];
        let total: u32 = nd.acts[x].iter().map(|a| a.n).sum();
        if total == 0 || nd.acts[x].len() == 1 {
            return plain(self, total);
        }
        // our turns worth weighing, the most visited first
        let mut ours: Vec<usize> = (0..nd.acts[x].len()).filter(|i| nd.acts[x][*i].n as u64 * 50 >= total as u64).collect();
        ours.sort_by_key(|i| std::cmp::Reverse(nd.acts[x][*i].n));
        ours.truncate(8);
        let rows = ours.len();
        if rows < 2 {
            return plain(self, total);
        }
        // how often we play each of them, as the search has it
        let mine: Vec<f32> = ours.iter().map(|i| nd.acts[x][*i].n as f32).collect();
        let mine_total: f32 = mine.iter().sum();
        let mine: Vec<f32> = mine.iter().map(|n| n / mine_total).collect();
        // the other fighter's turns: the shares the search gave them, and the network's
        let their_total: u32 = nd.acts[1 - x].iter().map(|a| a.n).sum();
        let expected: Vec<f32> = nd.acts[1 - x].iter().map(|a| a.n as f32 / their_total.max(1) as f32).collect();
        let prior: Vec<f32> = nd.acts[1 - x].iter().map(|a| a.prior).collect();
        let cols = expected.len();
        let value = if x == 0 { self.value() } else { -self.value() };
        let trust = trust.clamp(0.0, 1.0);
        let pair = |i: usize, j: usize| if x == 0 { (i, j) } else { (j, i) };
        // the table, our weighed turns by all of theirs: (mean result for us, lines). A pair
        // that ends the game is known outright and takes no lines.
        const KNOWN: u32 = u32::MAX / 4;
        let mut tab: Vec<Vec<(f32, u32)>> = Vec::with_capacity(rows);
        for &i in &ours {
            let row = (0..cols)
                .map(|j| {
                    let (i0, i1) = pair(i, j);
                    let mut s = *root;
                    self.e.step(&mut s, &[self.nodes[0].acts[0][i0].turn, self.nodes[0].acts[1][i1].turn]);
                    match outcome(&s) {
                        Some(v) => (if x == 0 { v } else { -v }, KNOWN),
                        None => self.cell(x, i, j),
                    }
                })
                .collect();
            tab.push(row);
        }
        let mut left = lines;
        // the lines run here, pair by pair: how many, their sum and their squares
        #[derive(Clone, Copy, Default)]
        struct Spread(f32, f32, f32);
        impl Spread {
            fn add(&mut self, v: f32) {
                self.0 += 1.0;
                self.1 += v;
                self.2 += v * v;
            }
        }
        let mut spread = vec![Spread::default(); rows * cols];
        // their best answer to our usual play
        let mut answer: Option<usize> = None;
        if trust < 1.0 {
            let budget = lines / 3;
            // a turn of theirs against our usual play: (our mean result over the pairs seen, lines)
            let column = |tab: &Vec<Vec<(f32, u32)>>, j: usize| {
                let (mut sum, mut seen, mut n) = (0.0f32, 0.0f32, 0u32);
                for r in 0..rows {
                    let c = tab[r][j];
                    if c.1 > 0 {
                        sum += mine[r] * c.0;
                        seen += mine[r];
                        n += if c.1 == KNOWN { 0 } else { c.1 };
                    }
                }
                (if seen > 0.0 { sum / seen } else { 0.0 }, seen, n)
            };
            for _ in 0..budget {
                let all: u32 = (0..cols).map(|j| column(&tab, j).2).sum();
                let root_n = (all.max(1) as f32).sqrt();
                let mut best = (f32::MIN, usize::MAX);
                for j in 0..cols {
                    if (0..rows).all(|r| tab[r][j].1 == KNOWN) {
                        continue;
                    }
                    let (mean, seen, n) = column(&tab, j);
                    // from their side: our result turned over
                    let q = if seen > 0.0 { -mean } else { -value - self.cfg.fpu };
                    let u = q + self.cfg.c * prior[j] * root_n / (1.0 + n as f32);
                    if u > best.0 {
                        best = (u, j);
                    }
                }
                if best.1 == usize::MAX {
                    break;
                }
                let j = best.1;
                // against the turn of ours whose few lines weigh most on the answer's result
                let r = (0..rows).filter(|r| tab[*r][j].1 != KNOWN).max_by(|a, b| (mine[*a] / (tab[*a][j].1 as f32 + 0.5)).partial_cmp(&(mine[*b] / (tab[*b][j].1 as f32 + 0.5))).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(0);
                let v = self.line(root, Some(pair(ours[r], j)));
                spread[r * cols + j].add(v);
                tab[r][j] = self.cell(x, ours[r], j);
                left -= 1;
            }
            // the answer: best for them, among the turns tried enough against most of our play
            let enough = (budget / 16).max(4);
            let mut best = (f32::MAX, usize::MAX);
            for j in 0..cols {
                let (mean, seen, n) = column(&tab, j);
                let known = (0..rows).all(|r| tab[r][j].1 == KNOWN);
                if seen >= 0.5 && (n >= enough || known) && mean < best.0 {
                    best = (mean, j);
                }
            }
            if best.1 != usize::MAX {
                answer = Some(best.1);
            }
        }
        // the mix our turns are compared against
        let mut mix: Vec<f32> = expected.clone();
        if let Some(j) = answer {
            for m in mix.iter_mut() {
                *m *= trust;
            }
            mix[j] += 1.0 - trust;
        }
        let score = |tab: &Vec<Vec<(f32, u32)>>, r: usize, mix: &[f32]| {
            let (mut sum, mut weight) = (0.0f32, 0.0f32);
            for j in 0..cols {
                if tab[r][j].1 > 0 {
                    sum += mix[j] * tab[r][j].0;
                    weight += mix[j];
                }
            }
            if weight > 0.0 { sum / weight } else { value }
        };
        // rounds of halving: the turns in the running share a round's lines evenly
        let mut running: Vec<usize> = (0..rows).collect();
        let mut dropped: Vec<Vec<usize>> = Vec::new();
        let rounds = (rows as f32).log2().ceil().max(1.0) as u32;
        let a_round = left / rounds;
        for round in 0..rounds {
            let each = a_round / running.len() as u32;
            for &r in &running {
                for _ in 0..each {
                    // the pair whose few lines weigh most on this turn's result
                    let mut best = (0.0f32, usize::MAX);
                    for j in 0..cols {
                        let c = tab[r][j];
                        if c.1 != KNOWN && mix[j] > 0.0 {
                            let u = mix[j] / (c.1 as f32 + 0.5);
                            if u > best.0 {
                                best = (u, j);
                            }
                        }
                    }
                    if best.1 == usize::MAX {
                        break;
                    }
                    let v = self.line(root, Some(pair(ours[r], best.1)));
                    spread[r * cols + best.1].add(v);
                    tab[r][best.1] = self.cell(x, ours[r], best.1);
                }
            }
            running.sort_by(|a, b| score(&tab, *b, &mix).partial_cmp(&score(&tab, *a, &mix)).unwrap_or(std::cmp::Ordering::Equal));
            // the last round leaves its turns in the order they came out
            if round + 1 < rounds && running.len() > 2 {
                let keep = (running.len() + 1) / 2;
                // the most visited turn (row 0) is never dropped
                if let Some(at) = running.iter().position(|r| *r == 0) {
                    if at >= keep {
                        running.swap(keep - 1, at);
                    }
                }
                dropped.push(running.split_off(keep));
            }
        }
        // Does the best turn beat the most visited one by more than the noise? The spread of
        // one line's result about its pair's mean, pooled over the pairs run here; a turn's
        // result is its pairs' means by the mix, each mean as sure as its lines are many.
        if running[0] != 0 {
            let (mut within, mut free) = (0.0f32, 0.0f32);
            for c in &spread {
                if c.0 >= 2.0 {
                    within += c.2 - c.1 * c.1 / c.0;
                    free += c.0 - 1.0;
                }
            }
            let var = if free >= 8.0 { within / free } else { 1.0 };
            let unsure = |r: usize| {
                let weight: f32 = (0..cols).filter(|j| tab[r][*j].1 > 0).map(|j| mix[j]).sum();
                (0..cols).filter(|j| tab[r][*j].1 > 0 && tab[r][*j].1 != KNOWN).map(|j| (mix[j] / weight.max(1e-9)).powi(2) * var / tab[r][j].1 as f32).sum::<f32>()
            };
            let gap = score(&tab, running[0], &mix) - score(&tab, 0, &mix);
            if gap < 3.0 * (unsure(running[0]) + unsure(0)).sqrt() {
                let at = running.iter().position(|r| *r == 0).unwrap_or(0);
                running[..=at].rotate_right(1);
            }
        }
        running.extend(dropped.into_iter().rev().flatten());
        running
            .into_iter()
            .map(|r| {
                let mean = score(&tab, r, &expected);
                let (worst, worst_to) = match answer {
                    Some(j) if tab[r][j].1 > 0 => (tab[r][j].0, self.nodes[0].acts[1 - x][j].turn),
                    Some(j) => (mean, self.nodes[0].acts[1 - x][j].turn),
                    None => (mean, RawTurn::IDLE),
                };
                WeighedTurn { turn: self.nodes[0].acts[x][ours[r]].turn, worth: score(&tab, r, &mix), expected: mean, worst, worst_to, lines: tab[r].iter().filter(|c| c.1 != KNOWN).map(|c| c.1).sum() }
            })
            .collect()
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
