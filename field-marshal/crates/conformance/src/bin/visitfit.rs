//! visitfit: trains the search's move model on its own visit counts (the AlphaZero recipe).
//!
//!   visitfit --target move|shot|frost|burst [--epochs 10] [--emit] <visits file>...
//!            [--replays dir --bots A,B,... --replay-weight 1]
//!
//! The move model of part 3: the deep search's choices (visits files from `teachdump --net`,
//! `<out>.visits`) and, with --replays, what the named strong bots actually played in the
//! replays (one-hot targets, by `movefeat::labels`, counted `--replay-weight` times).
//!
//! A visits file comes from `ARENA_VISITS=file arena ...`: per tree decision, the shares of
//! the root's visits by move class (hold, toward, away, sideways, dash) and the shares spent
//! on turns with a shot, a frostburst and a repulsor, then the 52 move features. The model
//! is the same multinomial logistic regression as `movefit` (for `move`; a yes/no model for
//! the attacks), fitted by SGD to the visit shares as soft targets. Every fifth line is held
//! out; the tool reports cross-entropy against the targets for the fit and for a constant.
//! With --emit it prints the weights as the Rust constant `movefit --emit` would.

use flate2::read::GzDecoder;
use std::io::Read;
use victris_engine::movefeat::{features, labels, CLASSES, N, TARGETS};
use victris_engine::replay::parse_replay;

fn softmax(w: &[[f32; N]; CLASSES], x: &[f32; N], k: usize) -> Vec<f32> {
    let mut z = vec![0.0f32; k];
    for c in 0..k {
        z[c] = w[c].iter().zip(x.iter()).map(|(a, b)| a * b).sum();
    }
    let m = z.iter().cloned().fold(f32::MIN, f32::max);
    let mut sum = 0.0;
    for v in z.iter_mut() {
        *v = (*v - m).exp();
        sum += *v;
    }
    z.iter().map(|v| v / sum).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let target = opt("--target").unwrap_or_else(|| "move".into());
    let epochs: usize = opt("--epochs").and_then(|s| s.parse().ok()).unwrap_or(10);
    let emit = args.iter().any(|a| a == "--emit");
    let k = if target == "move" { CLASSES } else { 2 };
    // --search-weight: how many times a decision of the search counts (its records are few)
    let search_weight = opt("--search-weight").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    let mut rows: Vec<([f32; N], Vec<f32>)> = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if ["--target", "--epochs", "--replays", "--bots", "--replay-weight", "--search-weight"].contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        let text = std::fs::read_to_string(a).unwrap_or_default();
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if it.next() != Some("V") {
                continue;
            }
            let v: Vec<f32> = it.filter_map(|x| x.parse().ok()).collect();
            if v.len() != 8 + N {
                continue;
            }
            let y: Vec<f32> = match target.as_str() {
                "move" => v[..5].to_vec(),
                "shot" => vec![1.0 - v[5], v[5]],
                "frost" => vec![1.0 - v[6], v[6]],
                "burst" => vec![1.0 - v[7], v[7]],
                _ => continue,
            };
            let mut f = [0.0f32; N];
            f.copy_from_slice(&v[8..]);
            for _ in 0..search_weight {
                rows.push((f, y.clone()));
            }
        }
    }
    let searched = rows.len() / search_weight;
    // the strong bots' own moves, one-hot
    let mut played = 0usize;
    if let Some(dir) = opt("--replays") {
        let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
        let weight = opt("--replay-weight").and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
        let which = TARGETS.iter().position(|n| *n == target.as_str()).unwrap_or(0);
        let k_idx = match target.as_str() { "move" => 0, "shot" => 2, "frost" => 3, "burst" => 4, _ => which };
        let mut files: Vec<_> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        for path in files {
            let mut text = String::new();
            if std::fs::File::open(&path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
            let Ok(replay) = parse_replay(&text) else { continue };
            let e = &replay.engine;
            let counted: Vec<bool> = (0..2).map(|i| replay.bots.get(i).map(|n| bots.contains(n)).unwrap_or(false)).collect();
            if !counted.iter().any(|c| *c) {
                continue;
            }
            let mut st = e.new_match(replay.seed);
            for t in &replay.ticks {
                if st.result.is_some() {
                    break;
                }
                for x in 0..2 {
                    if !counted[x] {
                        continue;
                    }
                    let Some(label) = labels(e, &st, x, &t.turns[x])[k_idx] else { continue };
                    let mut y = vec![0.0f32; k];
                    y[label.min(k - 1)] = 1.0;
                    let f = features(e, &st, x);
                    for _ in 0..weight {
                        rows.push((f, y.clone()));
                    }
                    played += 1;
                }
                e.step(&mut st, &t.turns);
            }
        }
    }
    let (mut train, mut test) = (Vec::new(), Vec::new());
    for (i, r) in rows.into_iter().enumerate() {
        if i % 5 == 0 { test.push(r) } else { train.push(r) }
    }
    if train.is_empty() {
        println!("no lines");
        return;
    }
    let mut w = [[0.0f32; N]; CLASSES];
    let mut order: Vec<usize> = (0..train.len()).collect();
    let mut seed = 0x9e3779b97f4a7c15u64;
    for epoch in 0..epochs {
        for i in (1..order.len()).rev() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            order.swap(i, (seed >> 33) as usize % (i + 1));
        }
        let lr = 0.05 / (1.0 + epoch as f32);
        for &i in &order {
            let (x, y) = &train[i];
            let p = softmax(&w, x, k);
            for c in 0..k {
                let g = p[c] - y[c];
                for j in 0..N {
                    w[c][j] -= lr * (g * x[j] + 1e-5 * w[c][j]);
                }
            }
        }
    }
    // held-out cross-entropy against the visit shares, for the fit and for the mean shares
    let mut mean = vec![0.0f32; k];
    for (_, y) in &train {
        for c in 0..k {
            mean[c] += y[c] / train.len() as f32;
        }
    }
    let ce = |p: &[f32], y: &[f32]| -> f64 { (0..k).map(|c| -(y[c] as f64) * (p[c].max(1e-6) as f64).ln()).sum() };
    let (mut fit, mut base) = (0.0, 0.0);
    for (x, y) in &test {
        fit += ce(&softmax(&w, x, k), y);
        base += ce(&mean, y);
    }
    let n = test.len().max(1) as f64;
    if !emit {
        println!("{} training decisions, {} held out ({searched} from the search, {played} strong-bot moves); cross-entropy against the targets: fit {:.4}, constant {:.4}", train.len(), test.len(), fit / n, base / n);
        return;
    }
    if target == "move" {
        println!("// Fitted by `visitfit` on the search's own visit counts ({} decisions); held-out cross-entropy {:.4} (constant {:.4})", train.len() + test.len(), fit / n, base / n);
        println!("pub const MOVE_OWN: [[f32; N]; CLASSES] = [");
        for c in 0..CLASSES {
            println!("    [{}],", w[c].iter().map(|v| format!("{v:.4}")).collect::<Vec<_>>().join(", "));
        }
        println!("];");
    } else {
        println!("// Fitted by `visitfit --target {target}` on the search's own visit counts ({} decisions)", train.len() + test.len());
        println!("pub const {}_OWN: [f32; N] = [{}];", target.to_uppercase(), (0..N).map(|j| format!("{:.4}", w[1][j] - w[0][j])).collect::<Vec<_>>().join(", "));
    }
}
