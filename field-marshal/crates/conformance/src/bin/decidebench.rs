//! decidebench: the search's decisions on fixed positions, deterministically, for the null test.
//!
//!   decidebench <replay dirs> [--max 500] [--every 30] [--iters 3000] [--gen 13]
//!
//! Positions of held-out games (a hash of the file name, % 10 < 2), every `--every` ticks from
//! tick 20, for each fighter that can act: a search with the flagship's leaf (linear judgment
//! row `--gen`, dash bursts, no buff value) and a fixed number of iterations. Prints one line per
//! decision: position number, fighter, the most visited turn and its visits. Written so it builds
//! on older trees too: run it on the live release's source and on the current one, then diff.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::parse_replay;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl.gz") {
        out.push(path.to_path_buf());
    }
}

fn fnv(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let max = num("--max", 500.0) as usize;
    let every = num("--every", 30.0) as usize;
    let iters = num("--iters", 3000.0) as u32;
    let gen = num("--gen", 13.0) as usize;
    let valued = ["--max", "--every", "--iters", "--gen"];
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if valued.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| seen.insert(p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    files.retain(|p| fnv(&p.file_name().unwrap().to_string_lossy()) % 10 < 2);
    let cfg = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst: true, ..Default::default() };
    let mut n = 0usize;
    for path in &files {
        if n >= max {
            break;
        }
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() || n >= max {
                break;
            }
            if ti % every == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive {
                n += 1;
                let mut search = Search::new(e, cfg);
                search.start();
                search.iterate(&s, [None, None], iters);
                let res = search.result();
                for x in 0..2 {
                    if let Some(best) = res.acts[x].iter().max_by_key(|a| a.1) {
                        println!("{n} {x} {:?} {}", best.0, best.1);
                    }
                }
            }
            e.step(&mut s, &t.turns);
        }
    }
}
