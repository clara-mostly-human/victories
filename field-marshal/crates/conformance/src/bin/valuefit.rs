//! valuefit: fits the judgment of positions to the search's own games.
//!
//!   valuefit [--ridge 1.0] [--basic] [--bots A,B,...] [--emit] <dump file or replay directory>...
//!
//! A dump file comes from `ARENA_DUMP=file arena ...`: one line per recorded tick, the
//! value features (fighter 0's vector minus fighter 1's, `victris_engine::valuefeat`)
//! followed by the discounted hp fighter 0 went on to gain or lose from there. A directory
//! holds replays (`ARENA_REPLAYS=dir arena ...`, .jsonl or .jsonl.gz): each game is played
//! again on the engine and every third tick recorded the same way (discount 0.98 a tick,
//! 40 for a kill), so the features can change without playing new games. The fit is a
//! ridge regression with no intercept (the judgment must be zero for mirrored positions).
//! Every fifth game is held out (every fifth line of a dump); the tool reports how much of
//! the outcome the judgment explains there, and with --emit prints the weights as a row
//! for `VALUE_GENS` in the engine's `models.rs`.

use flate2::read::GzDecoder;
use std::io::Read;
use victris_engine::replay::parse_replay;
use victris_engine::valuefeat::NV;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let ridge: f64 = opt("--ridge").and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let emit = args.iter().any(|a| a == "--emit");
    // --bots A,B,...: only replays in which both fighters are among these bots
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    // rows: (features, outcome, held out)
    let mut rows: Vec<([f64; NV], f64, bool)> = Vec::new();
    let mut games = 0usize;
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--ridge" || a == "--bots" {
            skip = true;
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        if std::path::Path::new(a).is_dir() {
            let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(a).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            files.sort();
            for path in files {
                let name = path.to_string_lossy().to_string();
                let mut text = String::new();
                if name.ends_with(".jsonl.gz") {
                    if std::fs::File::open(&path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                        continue;
                    }
                } else if name.ends_with(".jsonl") {
                    text = std::fs::read_to_string(&path).unwrap_or_default();
                } else {
                    continue;
                }
                let Ok(replay) = parse_replay(&text) else { continue };
                if !bots.is_empty() && !replay.bots.iter().all(|n| bots.contains(n)) {
                    continue;
                }
                games += 1;
                let e = &replay.engine;
                let mut s = e.new_match(replay.seed);
                let mut trace: Vec<([f32; NV], f64)> = Vec::new();
                for t in &replay.ticks {
                    if s.result.is_some() {
                        break;
                    }
                    let f = victris_engine::valuefeat::diff(e, &s, 0);
                    let before = (s.fighters[0].hp, s.fighters[1].hp);
                    e.step(&mut s, &t.turns);
                    let mut r = ((s.fighters[0].hp.max(0) - before.0) - (s.fighters[1].hp.max(0) - before.1)) as f64;
                    if !s.fighters[1].alive {
                        r += 40.0;
                    }
                    if !s.fighters[0].alive {
                        r -= 40.0;
                    }
                    trace.push((f, r));
                }
                let mut g = 0.0;
                for (i, (f, r)) in trace.iter().enumerate().rev() {
                    g = r + 0.98 * g;
                    if i % 3 == 0 {
                        let mut x = [0.0; NV];
                        for k in 0..NV {
                            x[k] = f[k] as f64;
                        }
                        rows.push((x, g, games % 5 == 0));
                    }
                }
            }
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
            let held = rows.len() % 5 == 0;
            rows.push((f, v[NV], held));
        }
    }
    // --basic: only the first features (those of the first generations), for comparison.
    if args.iter().any(|a| a == "--basic") {
        for row in rows.iter_mut() {
            for k in victris_engine::valuefeat::BASIC..NV {
                row.0[k] = 0.0;
            }
        }
    }
    // Normal equations on the training lines: (X'X + ridge I) w = X'y.
    let mut a = vec![vec![0.0f64; NV + 1]; NV];
    for (f, y, held) in rows.iter() {
        if *held {
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
        println!("    [{}],", w.iter().map(|v| format!("{v:.3}")).collect::<Vec<_>>().join(", "));
        return;
    }
    let (mut sse, mut sst, mut n) = (0.0, 0.0, 0usize);
    for (f, y, held) in rows.iter() {
        if !*held {
            continue;
        }
        let pred: f64 = f.iter().zip(&w).map(|(x, k)| x * k).sum();
        sse += (y - pred).powi(2);
        sst += y * y;
        n += 1;
    }
    println!("{} recorded ticks ({games} replayed games), {} held out; the judgment explains {:.1}% of the outcome on the held-out ticks (typical miss {:.1} hp, outcome spread {:.1} hp)", rows.len(), n, 100.0 * (1.0 - sse / sst.max(1e-9)), (sse / n.max(1) as f64).sqrt(), (sst / n.max(1) as f64).sqrt());
    let names = [
        "hp", "buff left", "shot ready", "shot cooldown", "dash ready", "dash cooldown", "frost ready", "repulsor ready", "can step", "cannot act", "slowed", "in their burst", "wall behind", "zone margin", "outside next zone",
        "near power pad", "near health pad when hurt", "their shot in the air", "close with shot ready", "close and stuck", "mid range with shot ready", "dash ready within 3", "hp squared",
        "slam on us is there", "slam on us one dash away", "sure shot ready", "far shot ready in sight", "one shot from dead", "two shots from dead", "nearer to a power pad", "nearer to a health pad when hurt",
    ];
    for (k, name) in names.iter().enumerate() {
        println!("  {:<28} {:>+8.2}", name, w[k]);
    }
}
