//! movefit: fits a model of how named bots move (hold, toward, away, sideways, dash) from
//! ladder replays, and says how well it predicts their moves in games it has not seen.
//!
//!   movefit --bots A,B,C [--target move|spent|shot|frost|burst] [--epochs 8] [--emit] <replay dir or files>...
//!
//! One example per tick on which a named bot could act, had its step ready and saw its
//! opponent. Features come from `victris_engine::movefeat`. The model is a multinomial
//! logistic regression fitted by stochastic gradient descent. Every fifth game is held
//! out. It reports accuracy and log loss on the held-out games for three predictors: the
//! most common move, the plain 12-cell table (range band x whose shot is ready), and the
//! fitted model. With --emit it prints the weights as a Rust constant.
//!
//! Targets other than `move` fit a yes/no model on the same features: `spent` = does it
//! dash when its step is spent; `shot`, `frost`, `burst` = does it use that attack on a
//! tick where the attack is ready (the repulsor only within 2 tiles, the frostburst only
//! within its range). They are reported against the base rate and the 12-cell table.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::movefeat::{features, move_class, table_cell, CLASSES, CLASS_NAMES, N};
use victris_engine::geom::chebyshev;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::FState;
use victris_engine::turn::{AttackIn, DirIn};

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

fn softmax(w: &[[f32; N]; CLASSES], x: &[f32; N]) -> [f32; CLASSES] {
    let mut z = [0.0f32; CLASSES];
    for c in 0..CLASSES {
        z[c] = w[c].iter().zip(x.iter()).map(|(a, b)| a * b).sum();
    }
    let m = z.iter().cloned().fold(f32::MIN, f32::max);
    let mut sum = 0.0;
    for c in 0..CLASSES {
        z[c] = (z[c] - m).exp();
        sum += z[c];
    }
    for c in 0..CLASSES {
        z[c] /= sum;
    }
    z
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let epochs: usize = opt("--epochs").and_then(|s| s.parse().ok()).unwrap_or(8);
    let emit = args.iter().any(|a| a == "--emit");
    let target = opt("--target").unwrap_or_else(|| "move".to_string());
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" || a == "--epochs" || a == "--target" {
            skip = true;
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let (mut train, mut test): (Vec<([f32; N], usize)>, Vec<([f32; N], usize)>) = (Vec::new(), Vec::new());
    let mut games = 0usize;
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let counted: Vec<bool> = (0..2).map(|i| replay.bots.get(i).map(|n| bots.contains(n)).unwrap_or(false)).collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        games += 1;
        let held_out = games % 5 == 0;
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            for x in 0..2 {
                if !counted[x] {
                    continue;
                }
                let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
                if f.state != FState::Idle || !can_see(&e.rules, &e.map, f.pos, o.pos) {
                    continue;
                }
                let d = chebyshev(f.pos, o.pos);
                // which attack of this kind is ready, if any
                let ready = |want: u8| {
                    e.rules.attacks.iter().enumerate().find(|(i, a)| {
                        f.cd_attack[*i] == 0
                            && match (&a.kind, want) {
                                (AttackKind::Projectile { .. }, 0) => true,
                                (AttackKind::Area { range, .. }, 1) => d <= *range,
                                (AttackKind::Burst { .. }, 2) => d <= 2,
                                _ => false,
                            }
                    }).map(|(i, _)| i)
                };
                let used = |i: usize| matches!(t.turns[x].attack, AttackIn::Named { attack, .. } if attack as usize == i) as usize;
                let label = match target.as_str() {
                    "move" if f.cd_move == 0 => Some(move_class(&s, x, &t.turns[x])),
                    "spent" if f.cd_move != 0 && f.cd_dash == 0 => Some(matches!(t.turns[x].dash, DirIn::Dir(_)) as usize),
                    "shot" => ready(0).map(used),
                    "frost" => ready(1).map(used),
                    "burst" => ready(2).map(used),
                    _ => None,
                };
                let Some(label) = label else { continue };
                let row = (features(e, &s, x), label);
                if held_out { test.push(row) } else { train.push(row) }
            }
            e.step(&mut s, &t.turns);
        }
    }
    // Baselines from the training games.
    let mut overall = [0u32; CLASSES];
    let mut cells = [[1u32; CLASSES]; 12]; // add-one smoothing
    for (x, y) in &train {
        overall[*y] += 1;
        cells[table_cell(x)][*y] += 1;
    }
    let total: u32 = overall.iter().sum();
    // Fit.
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
            let p = softmax(&w, x);
            for c in 0..CLASSES {
                let g = p[c] - (c == *y) as i32 as f32;
                for k in 0..N {
                    w[c][k] -= lr * (g * x[k] + 1e-5 * w[c][k]);
                }
            }
        }
    }
    if emit && target == "move" {
        println!("// Generated by `movefit --emit` from {} games of: {}", games, bots.join(", "));
        println!("// rows: {}; columns: the {} features of victris_engine::movefeat", CLASS_NAMES.join(", "), N);
        println!("pub const MOVE_WEIGHTS: [[f32; {N}]; {CLASSES}] = [");
        for c in 0..CLASSES {
            println!("    [{}],", w[c].iter().map(|v| format!("{v:.4}")).collect::<Vec<_>>().join(", "));
        }
        println!("];");
        return;
    }
    if emit {
        // A yes/no model: the log odds of "yes" are the difference of the two rows.
        println!("// Generated by `movefit --target {target} --emit` from {} games; log odds of yes over the {} features of movefeat", games, N);
        println!("pub const {}_WEIGHTS: [f32; {N}] = [{}];", target.to_uppercase(), (0..N).map(|k| format!("{:.4}", w[1][k] - w[0][k])).collect::<Vec<_>>().join(", "));
        return;
    }
    // Score the three predictors on the held-out games.
    let mut acc = [0u32; 3];
    let mut loss = [0.0f64; 3];
    let mut by_class = [[0u32; 2]; CLASSES]; // (count, model correct)
    let modal = (0..CLASSES).max_by_key(|c| overall[*c]).unwrap();
    for (x, y) in &test {
        let p0: Vec<f64> = overall.iter().map(|c| *c as f64 / total as f64).collect();
        let cell = cells[table_cell(x)];
        let cs: u32 = cell.iter().sum();
        let p1: Vec<f64> = cell.iter().map(|c| *c as f64 / cs as f64).collect();
        let p2: Vec<f64> = softmax(&w, x).iter().map(|v| *v as f64).collect();
        let arg = |p: &Vec<f64>| (0..CLASSES).max_by(|a, b| p[*a].partial_cmp(&p[*b]).unwrap()).unwrap();
        acc[0] += (modal == *y) as u32;
        acc[1] += (arg(&p1) == *y) as u32;
        acc[2] += (arg(&p2) == *y) as u32;
        loss[0] -= p0[*y].max(1e-9).ln();
        loss[1] -= p1[*y].max(1e-9).ln();
        loss[2] -= p2[*y].max(1e-9).ln();
        by_class[*y][0] += 1;
        by_class[*y][1] += (arg(&p2) == *y) as u32;
    }
    let n = test.len().max(1) as f64;
    println!("{} over {games} games: {} training decisions, {} held out", bots.join(", "), train.len(), test.len());
    if target == "move" {
        println!("moves in training: {}", (0..CLASSES).map(|c| format!("{} {:.0}%", CLASS_NAMES[c], 100.0 * overall[c] as f64 / total as f64)).collect::<Vec<_>>().join(", "));
    } else {
        println!("target {target}: yes on {:.1}% of training ticks where it was possible", 100.0 * overall[1] as f64 / total as f64);
    }
    println!("{:<34} {:>9} {:>9}", "predictor (on held-out games)", "accuracy", "log loss");
    for (i, name) in ["most common answer", "12-cell table", "fitted model"].iter().enumerate() {
        println!("{:<34} {:>8.1}% {:>9.3}", name, 100.0 * acc[i] as f64 / n, loss[i] / n);
    }
    let names: Vec<&str> = if target == "move" { CLASS_NAMES.to_vec() } else { vec!["no", "yes"] };
    println!(
        "fitted model, share of each real answer it predicts: {}",
        (0..names.len()).map(|c| format!("{} {:.0}% of {}", names[c], 100.0 * by_class[c][1] as f64 / by_class[c][0].max(1) as f64, by_class[c][0])).collect::<Vec<_>>().join(", ")
    );
}
