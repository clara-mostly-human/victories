//! arena: play two playbooks against each other on the exact engine.
//!
//!   arena [--pairs 50] [--seed 1] [--threads N] [--a file.json] [--b file.json | --b-clone]
//!
//! Each seed is played on all three maps with the seats both ways, so `--pairs 50` is 300
//! games. A playbook file holds the fields of `Playbook` to change, e.g. {"fire_cost": 2}.
//! With no files, both sides play the default and the result should be close to even.

use marshal::policy::Playbook;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let pairs: u32 = opt("--pairs").and_then(|s| s.parse().ok()).unwrap_or(50);
    let seed: i64 = opt("--seed").and_then(|s| s.parse().ok()).unwrap_or(1);
    let threads: usize = opt("--threads")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get().saturating_sub(2).max(1)).unwrap_or(4));
    let load = |flag: &str, name: &str| match opt(flag) {
        None => Playbook { name: name.to_string(), ..Playbook::default() },
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
            Playbook::from_json(name, &serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}")))
        }
    };
    let a = load("--a", "A");
    let b = load("--b", "B");
    let t0 = Instant::now();
    // --b-clone: B is the sparring partner cloned from the ladder's strong bots.
    let clone = args.iter().any(|x| x == "--b-clone");
    let t = marshal::arena::run_match_with(&a, if clone { None } else { Some(&b) }, pairs, seed, threads);
    let dt = t0.elapsed().as_secs_f64();
    let (score, half) = t.score();
    println!(
        "A {} - B {} - draws {} of {} games | A scores {:.1}% (95% interval {:.1}% to {:.1}%)",
        t.a_wins, t.b_wins, t.draws, t.games, 100.0 * score, 100.0 * (score - half), 100.0 * (score + half)
    );
    println!(
        "avg {} ticks a game | {:.0} games/s on {threads} threads ({:.0} bot decisions/s, {:.1}M engine ticks/s inside decisions) | tracker mismatches {}",
        t.ticks / t.games.max(1) as u64,
        t.games as f64 / dt,
        2.0 * t.ticks as f64 / dt,
        t.sims as f64 / dt / 1e6,
        t.mismatches
    );
    let g = t.games.max(1) as f64;
    for (name, x) in [("A", 0), ("B", 1)] {
        let v = |k: usize| t.stats[x][k] as f64 / g;
        println!(
            "{name} a game: dealt ranged {:.0}, frost {:.0}, repulsor {:.0}, slam {:.0}; zone taken {:.0}; bursts thrown {:.1}, dashes {:.1}, shots {:.1}, power pads {:.2}",
            v(0), v(1), v(2), v(3), v(4), v(5), v(6), v(7), v(8)
        );
    }
    // with `tree_pv_fog_ticks`: how good the guesses at the hidden opponent were
    let gs: Vec<f64> = marshal::policy::PV_GUESS_STATS.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed) as f64).collect();
    if gs[0] > 0.0 {
        println!(
            "guesses at the hidden opponent: {:.0} ticks a game decided over them, {:.1} guesses a tick; weight on the tile it really stood on {:.0}%; likeliest guess right {:.0}%, within one tile {:.0}%; the standing ghost right {:.0}%, within one tile {:.0}%",
            gs[0] / g, gs[6] / gs[0], gs[1] / gs[0] / 1e4, 100.0 * gs[2] / gs[0], 100.0 * gs[3] / gs[0], 100.0 * gs[4] / gs[0], 100.0 * gs[5] / gs[0]
        );
    }
    // with `tree_pv_fog_belief`: how the belief did, and how many hidden ticks it left alone
    if gs[8] > 0.0 {
        println!(
            "belief about the hidden opponent: moved on {:.0} ticks a game, {:.1} places; the guesses to search held {:.0}% of it and the opponent really stood on one of their tiles {:.0}%; {:.0} ticks a game left to the play without the switch; moving it on took {:.2} ms a tick; {:.1}% of the whole belief was on the tile the opponent really stood on",
            gs[8] / g, gs[12] / gs[8], gs[9] / gs[8] / 1e4, 100.0 * gs[10] / gs[8], gs[11] / g, gs[13] / gs[8] / 1e3, gs[14] / gs[8] / 1e4
        );
        let held: Vec<u64> = marshal::policy::PV_BELIEF_HELD.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).collect();
        let steps: Vec<String> = (0..10).filter(|i| held[2 * i] > 0).map(|i| format!("{}-{}%: {} ticks, really {:.0}%", 10 * i, 10 * i + 10, held[2 * i], 100.0 * held[2 * i + 1] as f64 / held[2 * i] as f64)).collect();
        println!("by the share the guesses held, how often the opponent really stood on one of their tiles: {}", steps.join("; "));
    }
}
