//! leafteach: targets for the judgment on the positions the search actually judges (part 2).
//!
//!   leafteach <replay dir>... --out file [--every 30] [--iters 3000] [--sample 600]
//!             [--deep 20000] [--depth 24] [--quiet 10] [--gen 13] [--shard k/n]
//!
//! For positions of the games `winlab` trains on (a hash of the file name, % 10 >= 3), every
//! `--every` ticks: a search like the flagship's (`--iters` iterations, linear judgment row
//! `--gen`) keeps every `--sample`-th leaf it judges. Each kept leaf is searched deeply (`--deep`
//! iterations, `--depth` + `--quiet` ticks ahead); its value is the hp the deep search expects
//! fighter 0 to gain from there. One line per leaf, in `teachdump --net`'s layout:
//! "L <hp lead> <linear judgment> <deep value> <the network's inputs for fighter 0, 1, position>".
//! Appends, and skips replays listed in `<out>.done`.

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let out = opt("--out").expect("--out file");
    let every = num("--every", 30.0) as usize;
    let iters = num("--iters", 3000.0) as u32;
    let sample = num("--sample", 600.0) as u32;
    let deep = num("--deep", 20000.0) as u32;
    let gen = num("--gen", 13.0) as usize;
    let (shard, shards) = opt("--shard").and_then(|s| s.split_once('/').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(1)))).unwrap_or((0usize, 1usize));
    let shallow = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst: true, max_nodes: 60_000, ..Default::default() };
    let deep_cfg = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst: true, max_nodes: 80_000, depth: num("--depth", 24.0) as u32, quiet: num("--quiet", 10.0) as u32, ..Default::default() };
    let valued = ["--out", "--every", "--iters", "--sample", "--deep", "--depth", "--quiet", "--gen", "--shard"];
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
    files.retain(|p| fnv(&p.file_name().unwrap().to_string_lossy()) % 10 >= 3);
    let done_path = format!("{out}.done");
    let done: std::collections::HashSet<String> = std::fs::read_to_string(&done_path).unwrap_or_default().lines().map(str::to_string).collect();
    let w = victris_engine::models::value_weights(gen);
    let mut written = 0usize;
    for (fi, path) in files.iter().enumerate() {
        if fi % shards.max(1) != shard {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if done.contains(&name) {
            continue;
        }
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
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
            if ti % every == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive {
                let mut search = Search::new(e, shallow);
                search.sample_every = sample;
                search.start();
                search.iterate(&s, [None, None], iters);
                let leaves = std::mem::take(&mut search.samples);
                for leaf in leaves {
                    let mut d = Search::new(e, deep_cfg);
                    d.start();
                    d.iterate(&leaf, [None, None], deep);
                    let v = d.result().value;
                    let lead = (leaf.fighters[0].hp - leaf.fighters[1].hp) as f32;
                    let jud = victris_engine::valuefeat::value(e, &leaf, 0, w);
                    lines.push_str(&format!("L {lead} {jud:.2} {v:.2}"));
                    for x in victris_engine::valuenet::own(e, &leaf, 0).iter().chain(victris_engine::valuenet::own(e, &leaf, 1).iter()).chain(victris_engine::valuenet::ctx(e, &leaf).iter()) {
                        lines.push_str(&format!(" {x:.3}"));
                    }
                    lines.push('\n');
                    written += 1;
                }
            }
            e.step(&mut s, &t.turns);
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).expect("open the output");
        let _ = f.write_all(lines.as_bytes());
        let mut dn = std::fs::OpenOptions::new().create(true).append(true).open(&done_path).expect("open the done list");
        let _ = writeln!(dn, "{name}");
        eprintln!("{name}: {written} leaves so far");
    }
}
