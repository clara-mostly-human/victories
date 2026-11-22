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
    /// per seat: damage dealt by ranged, area, burst, wall slam; zone damage taken;
    /// bursts thrown (areas); dashes; shots fired; power pads taken
    pub stats: [[u32; 9]; 2],
}

/// Plays one game. `books[i]` is the playbook of fighter index `i`.
pub fn play(engine: &Engine, seed: i64, books: [&Playbook; 2]) -> GameResult {
    play_with(engine, seed, [Some(books[0]), Some(books[1])])
}

/// Plays one game; a seat with no playbook is taken by the clone of the ladder's strong
/// bots (`victris_engine::clone`), which sees the true state and samples its turns.
pub fn play_with(engine: &Engine, seed: i64, books: [Option<&Playbook>; 2]) -> GameResult {
    let mut s = engine.new_match(seed);
    let mut trackers = [Tracker::for_engine(engine.clone(), 0, seed), Tracker::for_engine(engine.clone(), 1, seed)];
    let mut prev: [Option<RawTurn>; 2] = [None, None];
    let mut events = Vec::new();
    let mut out = GameResult::default();
    let mut rng = (seed as u64).wrapping_mul(0x9e3779b97f4a7c15) | 1;
    // ARENA_DUMP=file: for fitting the judgment of positions, every third tick is written
    // as the value features (fighter 0 minus fighter 1) and the discounted hp fighter 0
    // went on to gain or lose from there (discount 0.98 a tick, 40 for a kill).
    let dumping = std::env::var_os("ARENA_DUMP").is_some();
    // ARENA_REPLAYS=dir: every game is written there as a replay in the ladder's format
    // (header, a line per tick with both turns, result), so the replay tools can read the
    // search's own games. Plain .jsonl; gzip them afterwards for tools that expect .gz.
    let mut replay: Option<String> = std::env::var_os("ARENA_REPLAYS").map(|_| {
        let which = victris_engine::shipped::MAP_NAMES.iter().position(|n| *n == engine.map.name).unwrap_or(0);
        let rules: Value = serde_json::from_str(victris_engine::shipped::RULES_JSON).unwrap_or(Value::Null);
        let map: Value = serde_json::from_str(victris_engine::shipped::MAPS_JSON[which]).unwrap_or(Value::Null);
        let name = |i: usize| books[i].map(|b| b.name.clone()).unwrap_or_else(|| "clone".to_string());
        let header = serde_json::json!({
            "type": "header", "replay_format": 1, "engine": victris_engine::ENGINE_VERSION, "rules": rules, "map_data": map,
            "rules_hash": engine.rules.hash, "seed": seed, "bots": [{ "name": name(0) }, { "name": name(1) }],
        });
        format!("{header}\n")
    });
    let mut trace: Vec<([f32; victris_engine::valuefeat::NV], f64)> = Vec::new();
    while s.result.is_none() {
        let before = (s.fighters[0].hp, s.fighters[1].hp);
        let feats = if dumping { Some(victris_engine::valuefeat::diff(engine, &s, 0)) } else { None };
        let mut turns = [RawTurn::IDLE; 2];
        for i in 0..2 {
            let Some(book) = books[i] else {
                turns[i] = victris_engine::clone::clone_turn(engine, &s, i, &mut rng);
                continue;
            };
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
            // ARENA_ORACLE=name: that playbook's learned search is told the true state while
            // the opponent is out of sight (an experiment: the worth of a perfect guess). The
            // name is the playbook's own, which the arena binary sets to A or B, not the
            // file's: ARENA_ORACLE=A.
            let oracle = std::env::var("ARENA_ORACLE").map(|n| n == book.name).unwrap_or(false);
            crate::policy::PV_ORACLE.with(|o| *o.borrow_mut() = if oracle { Some(s) } else { None });
            crate::policy::PV_TRUTH.with(|o| *o.borrow_mut() = Some(s));
            let d = decide(&trackers[i], &view, book);
            crate::policy::PV_ORACLE.with(|o| *o.borrow_mut() = None);
            crate::policy::PV_TRUTH.with(|o| *o.borrow_mut() = None);
            if book.tree_pv_ponder > 0 && d.note == "pv" {
                crate::policy::pv_ponder(&trackers[i], book, book.tree_pv_ponder, None);
            }
            // ARENA_DASHLOG=1: one line for every dash, to see what the dashes are for.
            if matches!(d.turn.dash, victris_engine::turn::DirIn::Dir(_)) && std::env::var_os("ARENA_DASHLOG").is_some() {
                let (me, en) = (&s.fighters[i], &s.fighters[1 - i]);
                let frosted = s.areas().iter().any(|b| b.owner as usize != i && engine.map.in_blast(b.pos, b.radius, me.pos));
                let with = match d.turn.attack {
                    victris_engine::turn::AttackIn::Named { attack, .. } => engine.rules.attacks[attack as usize].name.as_str(),
                    _ => "-",
                };
                eprintln!(
                    "dash {} tick {} note '{}' with {} | enemy seen {} dist {} their dash cd {} | in their frost {} | hp {} vs {}",
                    book.name, view.tick, d.note, with, view.enemy.is_some(), victris_engine::geom::chebyshev(me.pos, en.pos), en.cd_dash, frosted, me.hp, en.hp
                );
            }
            out.sims += d.sims as u64;
            // ARENA_PRIORS=file: every turn the tree search chose, as a line per open
            // decision (which of the five, what it chose, the 52 move features), for
            // fitting the search's move ordering on its own choices (`movefit --dump`).
            if d.note == "tree" {
                if let Some(path) = std::env::var_os("ARENA_PRIORS") {
                    let labels = victris_engine::movefeat::labels(engine, &s, i, &d.turn);
                    if labels.iter().any(|l| l.is_some()) {
                        let feat = victris_engine::movefeat::features(engine, &s, i);
                        let tail: String = feat.iter().map(|v| format!(" {v:.3}")).collect();
                        let mut lines = String::new();
                        for (k, l) in labels.iter().enumerate() {
                            if let Some(l) = l {
                                lines.push_str(&format!("{} {}{}\n", victris_engine::movefeat::TARGETS[k], l, tail));
                            }
                        }
                        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                            let _ = std::io::Write::write_all(&mut file, lines.as_bytes());
                        }
                    }
                }
            }
            turns[i] = d.turn;
            prev[i] = Some(d.turn);
        }
        if let Some(text) = replay.as_mut() {
            let line = serde_json::json!({ "type": "tick", "tick": s.tick, "turns": {
                "1": crate::session::turn_json(s.tick, &turns[0], engine), "2": crate::session::turn_json(s.tick, &turns[1], engine) } });
            text.push_str(&format!("{line}\n"));
        }
        engine.step_events(&mut s, &turns, &mut events);
        if let Some(f) = feats {
            let mut r = ((s.fighters[0].hp.max(0) - before.0) - (s.fighters[1].hp.max(0) - before.1)) as f64;
            if !s.fighters[1].alive {
                r += 40.0;
            }
            if !s.fighters[0].alive {
                r -= 40.0;
            }
            trace.push((f, r));
        }
        for ev in &events {
            use victris_engine::events::Event;
            use victris_engine::rules::AttackKind;
            match ev {
                Event::Hit { attacker, attack, damage, .. } => {
                    let k = match engine.rules.attacks[*attack as usize].kind {
                        AttackKind::Projectile { .. } => 0,
                        AttackKind::Area { .. } => 1,
                        AttackKind::Burst { .. } => 2,
                    };
                    out.stats[*attacker as usize - 1][k] += *damage as u32;
                }
                Event::WallSlam { attacker, damage, .. } => out.stats[*attacker as usize - 1][3] += *damage as u32,
                Event::ZoneDamage { fighter, damage } => out.stats[*fighter as usize - 1][4] += *damage as u32,
                Event::AreaSpawn { owner, .. } => out.stats[*owner as usize - 1][5] += 1,
                Event::Dash { fighter, .. } => out.stats[*fighter as usize - 1][6] += 1,
                Event::ProjectileSpawn { owner, .. } => out.stats[*owner as usize - 1][7] += 1,
                Event::Pickup { fighter, kind: victris_engine::map::PadKind::Power, .. } => out.stats[*fighter as usize - 1][8] += 1,
                _ => {}
            }
        }
    }
    if dumping {
        use std::io::Write;
        let mut g = 0.0;
        let mut lines = String::new();
        for (i, (f, r)) in trace.iter().enumerate().rev() {
            g = r + 0.98 * g;
            if i % 3 == 0 {
                for v in f {
                    lines.push_str(&format!("{v:.3} "));
                }
                lines.push_str(&format!("{g:.2}\n"));
            }
        }
        if let Some(path) = std::env::var_os("ARENA_DUMP") {
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = file.write_all(lines.as_bytes());
            }
        }
    }
    let r = s.result.unwrap();
    if let (Some(mut text), Some(dir)) = (replay, std::env::var_os("ARENA_REPLAYS")) {
        static WRITTEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        text.push_str(&format!("{}\n", serde_json::json!({ "type": "result", "winner": r.winner, "reason": format!("{:?}", r.reason).to_lowercase(), "final_tick": r.final_tick })));
        let path = std::path::Path::new(&dir).join(format!("{}-{:04}-{}-{}.jsonl", std::process::id(), n, engine.map.name, seed));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(path, text);
    }
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
    /// the game statistics of `GameResult::stats`, summed for A and for B
    pub stats: [[u64; 9]; 2],
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
        for x in 0..2 {
            for k in 0..9 {
                self.stats[x][k] += o.stats[x][k];
            }
        }
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
    run_match_with(a, Some(b), pairs, first_seed, threads)
}

/// As `run_match`; with no `b`, A plays the clone of the ladder's strong bots.
pub fn run_match_with(a: &Playbook, b: Option<&Playbook>, pairs: u32, first_seed: i64, threads: usize) -> Tally {
    // ARENA_MAPS=columns-21,pillars-21 keeps only those maps (default: all three)
    let only: Vec<String> = std::env::var("ARENA_MAPS").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default();
    let engines: Vec<Engine> = victris_engine::shipped::MAP_NAMES.iter().filter(|n| only.is_empty() || only.iter().any(|o| o == *n)).map(|n| Engine::shipped(n)).collect();
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
                        let books = if swapped { [b, Some(a)] } else { [Some(a), b] };
                        let g = play_with(&engines[m], seed, books);
                        t.games += 1;
                        t.ticks += g.ticks as u64;
                        t.mismatches += g.mismatches;
                        t.sims += g.sims;
                        for seat in 0..2 {
                            let side = if (seat == 0) != swapped { 0 } else { 1 };
                            for k in 0..9 {
                                t.stats[side][k] += g.stats[seat][k] as u64;
                            }
                        }
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
