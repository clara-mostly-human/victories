//! teachdump: the teacher's targets. Positions from replays, each valued by a deep,
//! far-looking search, written in `valuefit`'s dump format so the judgment can be fitted
//! to what the deep search sees.
//!
//!   teachdump <replay dir> --out file [--every 6] [--deep 300000] [--depth 30] [--quiet 10]
//!             [--gen 6] [--dash-burst] [--max-nodes 120000]
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
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let done_path = format!("{out}.done");
    let done: std::collections::HashSet<String> = std::fs::read_to_string(&done_path).unwrap_or_default().lines().map(str::to_string).collect();
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let cfg = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst, max_nodes, depth, quiet, ..Default::default() };
    let mut written = 0usize;
    for path in files {
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
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut lines = String::new();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            let (a, b) = (&s.fighters[0], &s.fighters[1]);
            if ti % every == 0 && s.tick >= 20 && a.alive && b.alive && (a.state == FState::Idle || b.state == FState::Idle) {
                let mut search = Search::new(e, cfg);
                search.start();
                search.iterate(&s, [None, None], deep);
                let v = search.result().value;
                for x in diff(e, &s, 0) {
                    lines.push_str(&format!("{x:.3} "));
                }
                lines.push_str(&format!("{v:.2}\n"));
                written += 1;
            }
            e.step(&mut s, &t.turns);
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).expect("open the output");
        let _ = f.write_all(lines.as_bytes());
        let mut d = std::fs::OpenOptions::new().create(true).append(true).open(&done_path).expect("open the done list");
        let _ = writeln!(d, "{name}");
        eprintln!("{name}: {written} positions so far");
    }
}
