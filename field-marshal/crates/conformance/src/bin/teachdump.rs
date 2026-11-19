//! teachdump: the teacher's targets. Positions from replays, each valued by a deep,
//! far-looking search, written in `valuefit`'s dump format so the judgment can be fitted
//! to what the deep search sees.
//!
//!   teachdump <replay dir> --out file [--every 6] [--deep 300000] [--depth 30] [--quiet 10]
//!             [--gen 6] [--dash-burst] [--max-nodes 120000] [--net] [--bots A,B,...]
//!
//! One line per position: the value features (fighter 0's vector minus fighter 1's,
//! `valuefeat::diff`) and the deep search's average result at the root for fighter 0, in
//! hp. Appends, and skips replays already listed in `<out>.done`, so a run stopped by the
//! batch pause carries on. Single-threaded: run it under `taskpolicy -b`.

use flate2::read::GzDecoder;
use std::io::{Read, Write};
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::state::FState;
use victris_engine::valuefeat::diff;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let out = opt("--out").expect("--out file");
    let every: usize = opt("--every").and_then(|s| s.parse().ok()).unwrap_or(6).max(1);
    let deep: u32 = opt("--deep").and_then(|s| s.parse().ok()).unwrap_or(300_000);
    let depth: u32 = opt("--depth").and_then(|s| s.parse().ok()).unwrap_or(30);
    let quiet: u32 = opt("--quiet").and_then(|s| s.parse().ok()).unwrap_or(10);
    let gen: usize = opt("--gen").and_then(|s| s.parse().ok()).unwrap_or(6);
    let max_nodes: u32 = opt("--max-nodes").and_then(|s| s.parse().ok()).unwrap_or(120_000);
    let dash_burst = args.iter().any(|a| a == "--dash-burst");
    // --net: lines for the win-chance network instead: "N <winner for fighter 0> <deep value>
    // <the network's inputs for fighter 0, fighter 1 and the position>"
    let net = args.iter().any(|a| a == "--net");
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let done_path = format!("{out}.done");
    let done: std::collections::HashSet<String> = std::fs::read_to_string(&done_path).unwrap_or_default().lines().map(str::to_string).collect();
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let cfg = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst, max_nodes, depth, quiet, ..Default::default() };
    // --shard k/n: only every n-th replay, starting at k, so n processes can share the work
    let (shard, shards) = opt("--shard").and_then(|s| s.split_once('/').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(1)))).unwrap_or((0usize, 1usize));
    let mut written = 0usize;
    for (fi, path) in files.into_iter().enumerate() {
        if fi % shards.max(1) != shard {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if done.contains(&name) {
            continue;
        }
        let mut text = String::new();
        let ok = if name.ends_with(".gz") {
            std::fs::File::open(&path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_ok()
        } else {
            std::fs::read_to_string(&path).map(|t| text = t).is_ok()
        };
        if !ok {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        if !bots.is_empty() && !replay.bots.iter().all(|b| bots.contains(b)) {
            continue;
        }
        let e = &replay.engine;
        // the winner, from the end of the game
        let winner = {
            let mut s = e.new_match(replay.seed);
            for t in &replay.ticks {
                if s.result.is_some() {
                    break;
                }
                e.step(&mut s, &t.turns);
            }
            match s.result.and_then(|r| r.winner) {
                Some(w) => if w == 1 { 1.0f32 } else { 0.0 },
                None => continue,
            }
        };
        let mut s = e.new_match(replay.seed);
        let mut lines = String::new();
        // with --net, the deep search's root visits for each fighter that decides here, in
        // `visitfit`'s format, go to `<out>.visits` (the move model's targets)
        let mut visits = String::new();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            let (a, b) = (&s.fighters[0], &s.fighters[1]);
            if ti % every == 0 && s.tick >= 20 && a.alive && b.alive && (a.state == FState::Idle || b.state == FState::Idle) {
                let mut search = Search::new(e, cfg);
                search.start();
                search.iterate(&s, [None, None], deep);
                let root = search.result();
                let v = root.value;
                if net {
                    for x in 0..2 {
                        if s.fighters[x].state != FState::Idle {
                            continue;
                        }
                        let total: u32 = root.acts[x].iter().map(|a| a.1).sum();
                        if total == 0 {
                            continue;
                        }
                        let (mut mv, mut atk) = ([0.0f64; 5], [0.0f64; 3]);
                        for (turn, n, _) in &root.acts[x] {
                            let w = *n as f64 / total as f64;
                            mv[victris_engine::movefeat::move_class(&s, x, turn)] += w;
                            if let victris_engine::turn::AttackIn::Named { attack, .. } = turn.attack {
                                atk[match e.rules.attacks[attack as usize].kind {
                                    victris_engine::rules::AttackKind::Projectile { .. } => 0,
                                    victris_engine::rules::AttackKind::Area { .. } => 1,
                                    victris_engine::rules::AttackKind::Burst { .. } => 2,
                                }] += w;
                            }
                        }
                        visits.push('V');
                        for w in mv.iter().chain(atk.iter()) {
                            visits.push_str(&format!(" {w:.4}"));
                        }
                        for f in victris_engine::movefeat::features(e, &s, x).iter() {
                            visits.push_str(&format!(" {f:.3}"));
                        }
                        visits.push('\n');
                    }
                    lines.push_str(&format!("N {winner} {v:.2}"));
                    for x in victris_engine::valuenet::own(e, &s, 0).iter().chain(victris_engine::valuenet::own(e, &s, 1).iter()).chain(victris_engine::valuenet::ctx(e, &s).iter()) {
                        lines.push_str(&format!(" {x:.3}"));
                    }
                    lines.push('\n');
                } else {
                    for x in diff(e, &s, 0) {
                        lines.push_str(&format!("{x:.3} "));
                    }
                    lines.push_str(&format!("{v:.2}\n"));
                }
                written += 1;
            }
            e.step(&mut s, &t.turns);
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).expect("open the output");
        let _ = f.write_all(lines.as_bytes());
        if !visits.is_empty() {
            let mut fv = std::fs::OpenOptions::new().create(true).append(true).open(format!("{out}.visits")).expect("open the visits file");
            let _ = fv.write_all(visits.as_bytes());
        }
        let mut d = std::fs::OpenOptions::new().create(true).append(true).open(&done_path).expect("open the done list");
        let _ = writeln!(d, "{name}");
        eprintln!("{name}: {written} positions so far, {} here", lines.lines().count());
    }
}
