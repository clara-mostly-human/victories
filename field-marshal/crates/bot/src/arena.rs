//! A Rust-only arena: two playbooks play each other on the exact engine, each side seeing
//! only what the ladder would send it (its observation and its visible events) and going
//! through the same tracker and policy code as the live bot. No sockets, no JSON frames.
//!
//! This is the test bench for the search: candidate against current best, on every map,
//! both seats, many seeds.

use crate::policy::{decide, Playbook};
use crate::tracker::{Sync, Tracker};
use serde_json::Value;
use victris_engine::events::visible_events;
use victris_engine::observe::View;
use victris_engine::turn::RawTurn;
use victris_engine::Engine;

#[derive(Clone, Copy, Debug, Default)]
pub struct GameResult {
    /// winning seat (0 or 1), `None` for a draw
    pub winner: Option<usize>,
    pub ticks: i32,
    /// prediction mismatches of the two trackers (should stay zero)
    pub mismatches: u32,
    pub sims: u64,
}

/// Plays one game. `books[i]` is the playbook of fighter index `i`.
pub fn play(engine: &Engine, seed: i64, books: [&Playbook; 2]) -> GameResult {
    let mut s = engine.new_match(seed);
    let mut trackers = [Tracker::for_engine(engine.clone(), 0, seed), Tracker::for_engine(engine.clone(), 1, seed)];
    let mut prev: [Option<RawTurn>; 2] = [None, None];
    let mut events = Vec::new();
    let mut out = GameResult::default();
    while s.result.is_none() {
        let mut turns = [RawTurn::IDLE; 2];
        for i in 0..2 {
            let view = View::from_state(engine, &s, i);
            let told: Vec<Value> = visible_events(engine, &s, i, &events).iter().map(|e| e.to_json(engine)).collect();
            let sync = trackers[i].observe(&view, &told, prev[i]);
            if sync == Sync::Mismatch {
                out.mismatches += 1;
                if std::env::var_os("ARENA_DEBUG").is_some() {
                    eprintln!("mismatch: map {} seed {seed} tick {} fighter {}: {}\n  events {}", engine.map.name, view.tick, i + 1,
                        trackers[i].last_mismatch.clone().unwrap_or_default(), Value::Array(told.clone()));
                }
            }
            let d = decide(&trackers[i], &view, books[i]);
            out.sims += d.sims as u64;
            turns[i] = d.turn;
            prev[i] = Some(d.turn);
        }
        engine.step_events(&mut s, &turns, &mut events);
    }
    let r = s.result.unwrap();
    out.winner = r.winner.map(|w| (w - 1) as usize);
    out.ticks = r.final_tick + 1;
    out
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tally {
    pub games: u32,
    pub a_wins: u32,
    pub b_wins: u32,
    pub draws: u32,
    pub ticks: u64,
    pub mismatches: u32,
    pub sims: u64,
}

impl Tally {
    pub fn add(&mut self, o: &Tally) {
        self.games += o.games;
        self.a_wins += o.a_wins;
        self.b_wins += o.b_wins;
        self.draws += o.draws;
        self.ticks += o.ticks;
        self.mismatches += o.mismatches;
        self.sims += o.sims;
    }
    /// A's score per game (win 1, draw 1/2) and the half-width of its 95% interval.
    pub fn score(&self) -> (f64, f64) {
        let n = self.games.max(1) as f64;
        let mean = (self.a_wins as f64 + 0.5 * self.draws as f64) / n;
        let var = (self.a_wins as f64 * (1.0 - mean).powi(2) + self.b_wins as f64 * mean.powi(2) + self.draws as f64 * (0.5 - mean).powi(2)) / n;
        (mean, 1.96 * (var / n).sqrt())
    }
}

/// Plays `pairs` seeds on each shipped map, each seed twice with the seats swapped, so
/// spawn and seat effects cancel. Runs on `threads` threads.
pub fn run_match(a: &Playbook, b: &Playbook, pairs: u32, first_seed: i64, threads: usize) -> Tally {
    let engines: Vec<Engine> = victris_engine::shipped::MAP_NAMES.iter().map(|n| Engine::shipped(n)).collect();
    let jobs: Vec<(usize, i64, bool)> = (0..pairs)
        .flat_map(|k| (0..engines.len()).flat_map(move |m| [(m, first_seed + k as i64, false), (m, first_seed + k as i64, true)]))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut total = Tally::default();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut t = Tally::default();
                    loop {
                        let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&(m, seed, swapped)) = jobs.get(j) else { break };
                        let books = if swapped { [b, a] } else { [a, b] };
                        let g = play(&engines[m], seed, books);
                        t.games += 1;
                        t.ticks += g.ticks as u64;
                        t.mismatches += g.mismatches;
                        t.sims += g.sims;
                        match g.winner {
                            None => t.draws += 1,
                            Some(w) if (w == 0) != swapped => t.a_wins += 1,
                            Some(_) => t.b_wins += 1,
                        }
                    }
                    t
                })
            })
            .collect();
        for h in handles {
            total.add(&h.join().unwrap());
        }
    });
    total
}
