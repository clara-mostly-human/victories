//! settle: how early in a search the move it finally plays is already its top move.
//!
//!   settle <replay dirs> [--max 400] [--every 30] [--ms 150] [--strategy 0]
//!
//! On positions of held-out games, a flagship-like search (row 13 judgment, dash bursts, plus the
//! strategy layer at the weight given) runs for `--ms` of wall time in steps of 100 iterations.
//! At every step it notes the most visited turn of the side to move (fighter 0, when it can act).
//! Reports how many iterations a full search gets, and at 10%, 25%, 50% and 75% of the time how
//! often the top turn then is the one played at the end; and the share of searches whose final
//! turn was on top from the halfway point onward without changing again.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use victris_engine::mcts::{Config, Search};
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let max = num("--max", 400.0) as usize;
    let every = num("--every", 30.0) as usize;
    let ms = num("--ms", 150.0);
    let valued = ["--max", "--every", "--ms", "--strategy"];
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
    files.sort_by_key(|p| fnv(&format!("s{}", p.file_name().unwrap().to_string_lossy())));
    let cfg = Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, strategy: num("--strategy", 0.0), ..Default::default() };
    let marks = [0.10, 0.25, 0.50, 0.75];
    let (mut n, mut iters_sum) = (0usize, 0u64);
    let mut agree = [0usize; 4];
    let mut stable_half = 0usize;
    let mut first_settle_frac = Vec::new();
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
            if ti % every == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive && s.fighters[0].state == victris_engine::state::FState::Idle {
                let mut search = Search::new(e, cfg);
                search.start();
                let t0 = Instant::now();
                let mut trace: Vec<(f64, RawTurn)> = Vec::new();
                let mut done = 0u64;
                while t0.elapsed().as_secs_f64() * 1000.0 < ms {
                    search.iterate(&s, [None, None], 100);
                    done += 100;
                    if let Some(best) = search.result().acts[0].iter().max_by_key(|a| a.1) {
                        trace.push((t0.elapsed().as_secs_f64() * 1000.0 / ms, best.0));
                    }
                }
                let Some(&(_, fin)) = trace.last() else { continue };
                n += 1;
                iters_sum += done;
                for (k, m) in marks.iter().enumerate() {
                    if let Some((_, tm)) = trace.iter().find(|(f, _)| *f >= *m) {
                        agree[k] += (*tm == fin) as usize;
                    }
                }
                if trace.iter().filter(|(f, _)| *f >= 0.5).all(|(_, tm)| *tm == fin) {
                    stable_half += 1;
                }
                // the time after which the final turn stays on top
                let mut settle = 1.0;
                for (f, tm) in trace.iter().rev() {
                    if *tm != fin {
                        break;
                    }
                    settle = *f;
                }
                first_settle_frac.push(settle);
            }
            e.step(&mut s, &t.turns);
        }
    }
    first_settle_frac.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| first_settle_frac.get(((first_settle_frac.len() as f64 - 1.0) * p) as usize).cloned().unwrap_or(0.0);
    println!("{n} searches of {ms} ms ({} iterations on average)", iters_sum / n.max(1) as u64);
    for (k, m) in marks.iter().enumerate() {
        println!("  at {:>3.0}% of the time the top turn is already the one played: {:.0}%", m * 100.0, 100.0 * agree[k] as f64 / n.max(1) as f64);
    }
    println!("  on top from halfway to the end without changing: {:.0}%", 100.0 * stable_half as f64 / n.max(1) as f64);
    println!("  time after which the played turn stays on top: median {:.0}%, a quarter of searches after {:.0}%, a tenth after {:.0}%", 100.0 * q(0.5), 100.0 * q(0.75), 100.0 * q(0.9));
}
