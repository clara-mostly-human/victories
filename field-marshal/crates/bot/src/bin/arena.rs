//! arena: play two playbooks against each other on the exact engine.
//!
//!   arena [--pairs 50] [--seed 1] [--threads N] [--a file.json] [--b file.json]
//!
//! Each seed is played on all three maps with the seats both ways, so `--pairs 50` is 300
//! games. A playbook file holds the fields of `Playbook` to change, e.g. {"fire_cost": 2}.
//! With no files, both sides play the default and the result should be close to even.

use marshal::arena::run_match;
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
    let t = run_match(&a, &b, pairs, seed, threads);
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
}
