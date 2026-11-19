//! visitfit: trains the search's move model on its own visit counts (the AlphaZero recipe).
//!
//!   visitfit --target move|shot|frost|burst [--epochs 10] [--emit] <visits file>...
//!
//! A visits file comes from `ARENA_VISITS=file arena ...`: per tree decision, the shares of
//! the root's visits by move class (hold, toward, away, sideways, dash) and the shares spent
//! on turns with a shot, a frostburst and a repulsor, then the 52 move features. The model
//! is the same multinomial logistic regression as `movefit` (for `move`; a yes/no model for
//! the attacks), fitted by SGD to the visit shares as soft targets. Every fifth line is held
//! out; the tool reports cross-entropy against the targets for the fit and for a constant.
//! With --emit it prints the weights as the Rust constant `movefit --emit` would.

use victris_engine::movefeat::{CLASSES, N};

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
    let mut rows: Vec<([f32; N], Vec<f32>)> = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--target" || a == "--epochs" {
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
            rows.push((f, y));
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
        println!("{} training decisions, {} held out; cross-entropy against the visit shares: fit {:.4}, constant {:.4}", train.len(), test.len(), fit / n, base / n);
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
