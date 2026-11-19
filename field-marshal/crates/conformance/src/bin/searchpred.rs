//! searchpred: how well a search, with a given leaf judgment, predicts who goes on to win.
//!
//!   searchpred <replay dirs> [--every 30] [--max 1500] [--iters 5000] [--depth 12] [--quiet 6]
//!              [--gen 13] [--lin 1.0] [--net 0] [--dash-burst] [--out file]
//!
//! Positions from the games `winlab` holds out (a hash of the file name, % 10 < 2), every
//! `--every` ticks from tick 20 while both fighters live, up to `--max`. Each is searched with
//! the leaf judgment given (`--lin`: weight of the linear judgment row `--gen`; `--net`: weight
//! in hp of the win-probability network compiled into the engine). The prediction for fighter
//! 0 is sigmoid(k * (hp lead + the search's root value)): k is fitted on the even-numbered
//! games and accuracy is reported on the odd ones, by stage of the game, next to hp alone on
//! the same positions. --out keeps "P <game> <tick> <last tick> <winner> <hp lead> <value>".

use flate2::read::GzDecoder;
use std::io::{Read, Write};
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

fn sigmoid(z: f64) -> f64 {
    1.0 / (1.0 + (-z).exp())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let every = num("--every", 30.0) as usize;
    let max = num("--max", 1500.0) as usize;
    let iters = num("--iters", 5000.0) as u32;
    let cfg = Config {
        value_weight: num("--lin", 1.0),
        value_gen: num("--gen", 13.0) as usize,
        net_weight: num("--net", 0.0),
        net_hp: num("--net-hp", 0.0),
        depth: num("--depth", 12.0) as u32,
        quiet: num("--quiet", 6.0) as u32,
        buff_value: 0.0,
        dash_burst: args.iter().any(|a| a == "--dash-burst"),
        max_nodes: 60_000,
        ..Default::default()
    };
    let valued = ["--net-hp", "--every", "--max", "--iters", "--lin", "--gen", "--net", "--depth", "--quiet", "--out"];
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
    // spread the sample over the held-out games: shuffle them by another hash
    files.sort_by_key(|p| fnv(&format!("x{}", p.file_name().unwrap().to_string_lossy())));
    // (game, tick, last, winner for fighter 0, hp lead, root value)
    let mut rows: Vec<(usize, i32, i32, f64, f64, f64)> = Vec::new();
    let t0 = std::time::Instant::now();
    for (gi, path) in files.iter().enumerate() {
        if rows.len() >= max {
            break;
        }
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            e.step(&mut s, &t.turns);
        }
        let Some(w) = s.result.and_then(|r| r.winner) else { continue };
        let y = if w == 1 { 1.0 } else { 0.0 };
        let last = s.tick;
        let mut s = e.new_match(replay.seed);
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() || rows.len() >= max {
                break;
            }
            if ti % every == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive {
                let mut search = Search::new(e, cfg);
                search.start();
                search.iterate(&s, [None, None], iters);
                let lead = (s.fighters[0].hp - s.fighters[1].hp) as f64;
                rows.push((gi, s.tick, last, y, lead, search.result().value));
            }
            e.step(&mut s, &t.turns);
        }
    }
    eprintln!("{} positions from held-out games in {:.0}s", rows.len(), t0.elapsed().as_secs_f64());
    if let Some(out) = opt("--out") {
        let mut f = std::fs::File::create(out).expect("create --out");
        for r in &rows {
            let _ = writeln!(f, "P {} {} {} {} {} {:.3}", r.0, r.1, r.2, r.3, r.4, r.5);
        }
    }
    let fit = |x: &dyn Fn(&(usize, i32, i32, f64, f64, f64)) -> f64| -> f64 {
        let mut k = 0.0f64;
        for _ in 0..100 {
            let (mut g, mut h) = (0.0, 1e-9);
            for r in rows.iter().filter(|r| r.0 % 2 == 0) {
                let v = x(r);
                let p = sigmoid(k * v);
                g += (p - r.3) * v;
                h += p * (1.0 - p) * v * v;
            }
            k -= g / h;
        }
        k
    };
    let search_x = |r: &(usize, i32, i32, f64, f64, f64)| r.4 + r.5;
    let hp_x = |r: &(usize, i32, i32, f64, f64, f64)| r.4;
    let (ks, kh) = (fit(&search_x), fit(&hp_x));
    let mut cells = [[0.0f64; 5]; 5]; // per stage and all: n, search acc, search ll, hp acc, hp ll
    for r in rows.iter().filter(|r| r.0 % 2 == 1) {
        let stage = ((r.1 as f64 / r.2.max(1) as f64) * 4.0).floor().clamp(0.0, 3.0) as usize;
        for c in [stage, 4] {
            cells[c][0] += 1.0;
            for (k, p) in [sigmoid(ks * search_x(r)), sigmoid(kh * hp_x(r))].iter().enumerate() {
                let acc = if (p - 0.5).abs() < 1e-9 { 0.5 } else { ((*p > 0.5) == (r.3 > 0.5)) as i32 as f64 };
                let pc = p.clamp(1e-6, 1.0 - 1e-6);
                cells[c][1 + 2 * k] += acc;
                cells[c][2 + 2 * k] += -(r.3 * pc.ln() + (1.0 - r.3) * (1.0 - pc).ln());
            }
        }
    }
    println!("search {} iterations, depth {} + quiet {}, linear {} net {} net-hp {}: accuracy and log loss on odd held-out games", iters, cfg.depth, cfg.quiet, cfg.value_weight, cfg.net_weight, cfg.net_hp);
    for (k, c) in cells.iter().enumerate() {
        let n = c[0].max(1.0);
        let label = if k < 4 { format!("stage {}/4", k + 1) } else { "all".to_string() };
        println!("{label:<10} {:>5}  search {:>5.1}% {:.3}  hp alone {:>5.1}% {:.3}", c[0] as usize, 100.0 * c[1] / n, c[2] / n, 100.0 * c[3] / n, c[4] / n);
    }
}
