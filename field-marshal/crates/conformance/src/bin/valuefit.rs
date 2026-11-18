//! valuefit: fits the judgment of positions to the search's own games.
//!
//!   valuefit [--ridge 1.0] [--emit] <dump file>...
//!
//! A dump file comes from `ARENA_DUMP=file arena ...`: one line per recorded tick, the
//! value features (fighter 0's vector minus fighter 1's, `victris_engine::valuefeat`)
//! followed by the discounted hp fighter 0 went on to gain or lose from there. The fit is
//! a ridge regression with no intercept (the judgment must be zero for mirrored
//! positions). Every fifth line is held out; the tool reports how much of the outcome the
//! judgment explains on those lines, and with --emit prints the weights as a Rust constant.

use victris_engine::valuefeat::NV;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let ridge: f64 = opt("--ridge").and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let emit = args.iter().any(|a| a == "--emit");
    let mut rows: Vec<([f64; NV], f64)> = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--ridge" {
            skip = true;
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        let text = std::fs::read_to_string(a).unwrap_or_else(|e| panic!("{a}: {e}"));
        for line in text.lines() {
            let v: Vec<f64> = line.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() != NV + 1 {
                continue;
            }
            let mut f = [0.0; NV];
            f.copy_from_slice(&v[..NV]);
            rows.push((f, v[NV]));
        }
    }
    // Normal equations on the training lines: (X'X + ridge I) w = X'y.
    let mut a = vec![vec![0.0f64; NV + 1]; NV];
    for (i, (f, y)) in rows.iter().enumerate() {
        if i % 5 == 0 {
            continue;
        }
        for r in 0..NV {
            for c in 0..NV {
                a[r][c] += f[r] * f[c];
            }
            a[r][NV] += f[r] * y;
        }
    }
    for (r, row) in a.iter_mut().enumerate() {
        row[r] += ridge;
    }
    // Gaussian elimination with partial pivoting.
    for col in 0..NV {
        let pivot = (col..NV).max_by(|x, y| a[*x][col].abs().partial_cmp(&a[*y][col].abs()).unwrap()).unwrap();
        a.swap(col, pivot);
        let p = a[col][col];
        if p.abs() < 1e-12 {
            continue;
        }
        for c in col..=NV {
            a[col][c] /= p;
        }
        for r in 0..NV {
            if r != col {
                let k = a[r][col];
                if k != 0.0 {
                    for c in col..=NV {
                        a[r][c] -= k * a[col][c];
                    }
                }
            }
        }
    }
    let w: Vec<f64> = (0..NV).map(|r| a[r][NV]).collect();
    if emit {
        println!("// Fitted by `valuefit` on {} recorded ticks of the search's own games.", rows.len());
        println!("pub const VALUE_WEIGHTS: [f32; crate::valuefeat::NV] = [{}];", w.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(", "));
        return;
    }
    let (mut sse, mut sst, mut n) = (0.0, 0.0, 0usize);
    for (i, (f, y)) in rows.iter().enumerate() {
        if i % 5 != 0 {
            continue;
        }
        let pred: f64 = f.iter().zip(&w).map(|(x, k)| x * k).sum();
        sse += (y - pred).powi(2);
        sst += y * y;
        n += 1;
    }
    println!("{} recorded ticks, {} held out; the judgment explains {:.1}% of the outcome on the held-out ticks (typical miss {:.1} hp, outcome spread {:.1} hp)", rows.len(), n, 100.0 * (1.0 - sse / sst.max(1e-9)), (sse / n.max(1) as f64).sqrt(), (sst / n.max(1) as f64).sqrt());
    let names = [
        "hp", "buff left", "shot ready", "shot cooldown", "dash ready", "dash cooldown", "frost ready", "repulsor ready", "can step", "cannot act", "slowed", "in their burst", "wall behind", "zone margin", "outside next zone",
        "near power pad", "near health pad when hurt", "their shot in the air", "close with shot ready", "close and stuck", "mid range with shot ready", "dash ready within 3", "hp squared",
    ];
    for (k, name) in names.iter().enumerate() {
        println!("  {:<28} {:>+8.2}", name, w[k]);
    }
}
