//! stratfit: what each mode's goal score of the strategy layer is worth in hp, beyond the judgment.
//!
//!   stratfit <replay dirs> [--gen 13] [--bots A,B]
//!
//! Every third tick (after tick 20): each fighter's mode (`strategy::choose`) and its goal score
//! (`strategy::score`, capped at 6), as five features, one per mode: fighter 0's score if it is in
//! that mode minus fighter 1's if it is. Least squares of (discounted hp to come - judgment row
//! `--gen`) on them, fitted on even-numbered files and checked on odd ones. A weight near 0 means
//! the judgment already counts that goal; a negative one means chasing it costs.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::replay::parse_replay;
use victris_engine::strategy;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl.gz") || path.to_string_lossy().ends_with(".jsonl") {
        out.push(path.to_path_buf());
    }
}

const K: usize = 5;

fn solve(mut a: [[f64; K]; K], mut b: [f64; K]) -> [f64; K] {
    for i in 0..K {
        a[i][i] += 1e-6;
    }
    for c in 0..K {
        let p = (c..K).max_by(|&i, &j| a[i][c].abs().partial_cmp(&a[j][c].abs()).unwrap()).unwrap();
        a.swap(c, p);
        b.swap(c, p);
        for r in 0..K {
            if r != c && a[c][c].abs() > 1e-12 {
                let f = a[r][c] / a[c][c];
                for k in 0..K {
                    a[r][k] -= f * a[c][k];
                }
                b[r] -= f * b[c];
            }
        }
    }
    let mut x = [0.0; K];
    for i in 0..K {
        x[i] = if a[i][i].abs() > 1e-12 { b[i] / a[i][i] } else { 0.0 };
    }
    x
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let gen: usize = opt("--gen").and_then(|s| s.parse().ok()).unwrap_or(13);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" || a == "--gen" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let w = victris_engine::models::value_weights(gen);
    // (test set?, features, residual)
    let mut rows: Vec<(bool, [f64; K], f64)> = Vec::new();
    let mut mode_count = [0usize; K];
    for (fi, path) in files.iter().enumerate() {
        let mut text = String::new();
        if path.to_string_lossy().ends_with(".gz") {
            if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else {
            text = std::fs::read_to_string(path).unwrap_or_default();
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        if !bots.is_empty() && !replay.bots.iter().all(|b| bots.contains(b)) {
            continue;
        }
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut rec: Vec<(usize, [f64; K], f64)> = Vec::new();
        let mut reward: Vec<f64> = Vec::new();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            if ti % 3 == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive {
                let mut f = [0.0f64; K];
                for x in 0..2 {
                    let m = strategy::choose(e, &s, x);
                    let k = strategy::mode_index(m);
                    mode_count[k] += 1;
                    let sc = strategy::score(e, &s, x, m, 6.0);
                    f[k] += if x == 0 { sc } else { -sc };
                }
                let j = victris_engine::valuefeat::value(e, &s, 0, w) as f64;
                rec.push((ti, f, j));
            }
            let before = (s.fighters[0].hp.max(0), s.fighters[1].hp.max(0));
            e.step(&mut s, &t.turns);
            let mut r = ((s.fighters[0].hp.max(0) - before.0) - (s.fighters[1].hp.max(0) - before.1)) as f64;
            if !s.fighters[1].alive {
                r += 40.0;
            }
            if !s.fighters[0].alive {
                r -= 40.0;
            }
            reward.push(r);
        }
        let mut to_come = vec![0.0f64; reward.len() + 1];
        for k in (0..reward.len()).rev() {
            to_come[k] = reward[k] + 0.98 * to_come[k + 1];
        }
        for (ti, f, j) in rec {
            rows.push((fi % 2 == 1, f, to_come[ti] - j));
        }
    }
    let (mut a, mut b) = ([[0.0f64; K]; K], [0.0f64; K]);
    for (test, f, y) in &rows {
        if *test {
            continue;
        }
        for i in 0..K {
            for j in 0..K {
                a[i][j] += f[i] * f[j];
            }
            b[i] += f[i] * y;
        }
    }
    let wk = solve(a, b);
    let (mut sse0, mut sse1, mut n) = (0.0, 0.0, 0usize);
    for (test, f, y) in &rows {
        if !*test {
            continue;
        }
        let p: f64 = (0..K).map(|i| wk[i] * f[i]).sum();
        sse0 += y * y;
        sse1 += (y - p) * (y - p);
        n += 1;
    }
    let total: usize = mode_count.iter().sum();
    println!("{} positions from {} files (judgment row {gen}); modes: {}", rows.len(), files.len(), (0..K).map(|k| format!("{} {:.0}%", strategy::MODE_NAMES[k], 100.0 * mode_count[k] as f64 / total.max(1) as f64)).collect::<Vec<_>>().join(", "));
    println!("worth in hp per unit of goal score, beyond the judgment: {}", (0..K).map(|k| format!("{} {:+.2}", strategy::MODE_NAMES[k], wk[k])).collect::<Vec<_>>().join(", "));
    println!("held-out ({n} positions): residual spread {:.2} hp without, {:.2} with ({:+.2}% of the residual explained)", (sse0 / n.max(1) as f64).sqrt(), (sse1 / n.max(1) as f64).sqrt(), 100.0 * (1.0 - sse1 / sse0.max(1e-9)));
}
