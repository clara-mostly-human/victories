//! leafcheck: two quick checks of the hp-to-come network as a leaf term, no games played.
//!
//!   leafcheck <replay dirs> [--max 300] [--every 30] [--iters 3000] [--gen 13]
//!
//! On positions of held-out games (a hash of the file name, % 10 < 2):
//! 1. Jitter: every turn fighter 0 could take, against fighter 1 standing still, stepped one
//!    tick; for those sibling positions, the spread (max - min and standard deviation, in hp)
//!    of the linear judgment and of the network's term, next to a 12-hp shot.
//! 2. Scale: a search with the flagship's leaf and one with the network added: the spread of
//!    root values (standard deviation of the average result of turns with 20+ visits) and the
//!    share of visits on the most visited turn.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::mcts::{actions_with, Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::turn::RawTurn;

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

fn stats(v: &[f64]) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    let n = v.len() as f64;
    let m = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n).sqrt();
    let range = v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min);
    (range, sd)
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let max = num("--max", 300.0) as usize;
    let every = num("--every", 30.0) as usize;
    let iters = num("--iters", 3000.0) as u32;
    let gen = num("--gen", 13.0) as usize;
    let cap = num("--cap", 0.0);
    let valued = ["--max", "--every", "--iters", "--gen", "--cap"];
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
    files.retain(|p| fnv(&p.file_name().unwrap().to_string_lossy()) % 10 < 2);
    files.sort_by_key(|p| fnv(&format!("x{}", p.file_name().unwrap().to_string_lossy())));
    let w = victris_engine::models::value_weights(gen);
    let base = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst: true, max_nodes: 60_000, ..Default::default() };
    let with_net = Config { net_value: 1.0, net_cap: cap, ..base };
    let (mut jr, mut js, mut nr, mut ns, mut over) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0usize);
    let (mut spread0, mut spread1, mut top0, mut top1) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut positions = 0usize;
    for path in &files {
        if positions >= max {
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
            if s.result.is_some() || positions >= max {
                break;
            }
            if ti % every == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive && s.fighters[0].state == victris_engine::state::FState::Idle {
                positions += 1;
                // 1. jitter between siblings
                let (mut jv, mut nv) = (Vec::new(), Vec::new());
                for a in actions_with(e, &s, 0, 0.25, false, false, false, true) {
                    let mut c = s;
                    e.step(&mut c, &[a.turn, RawTurn::IDLE]);
                    if c.result.is_some() {
                        continue;
                    }
                    jv.push(victris_engine::valuefeat::value(e, &c, 0, w) as f64);
                    let t = victris_engine::valuenet::hp_to_come(e, &c, 0, &victris_engine::valuenet_hp_weights::VALUE_HP_NET) as f64;
                    nv.push(if cap > 0.0 { t.clamp(-cap, cap) } else { t });
                }
                let (a1, b1) = stats(&jv);
                let (a2, b2) = stats(&nv);
                jr.push(a1);
                js.push(b1);
                nr.push(a2);
                ns.push(b2);
                if a2 > 12.0 {
                    over += 1;
                }
                // 2. scale: root value spread and top visit share, without and with the network
                for (cfg, spread, top) in [(base, &mut spread0, &mut top0), (with_net, &mut spread1, &mut top1)] {
                    let mut search = Search::new(e, cfg);
                    search.start();
                    search.iterate(&s, [None, None], iters);
                    let res = search.result();
                    let acts = &res.acts[0];
                    let total: u32 = acts.iter().map(|a| a.1).sum();
                    let best = acts.iter().map(|a| a.1).max().unwrap_or(0);
                    top.push(best as f64 / total.max(1) as f64);
                    let qs: Vec<f64> = acts.iter().filter(|a| a.1 >= 20).map(|a| a.2).collect();
                    spread.push(stats(&qs).1);
                }
            }
            e.step(&mut s, &t.turns);
        }
    }
    println!("{positions} positions of held-out games");
    println!("jitter between sibling positions (fighter 0's turns, fighter 1 standing still), median over positions:");
    println!("  linear judgment: range {:.1} hp, sd {:.1} hp", median(jr), median(js));
    println!("  network term:    range {:.1} hp, sd {:.1} hp; range above a 12-hp shot at {:.1}% of positions", median(nr), median(ns), 100.0 * over as f64 / positions.max(1) as f64);
    println!("scale at the root ({iters} iterations), median over positions:");
    println!("  flagship leaf:   spread of turn values {:.1} hp, top turn's share of visits {:.1}%", median(spread0), 100.0 * median(top0));
    println!("  with network:    spread of turn values {:.1} hp, top turn's share of visits {:.1}%", median(spread1), 100.0 * median(top1));
}
