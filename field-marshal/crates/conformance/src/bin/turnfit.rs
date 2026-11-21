//! turnfit: fits the turn model (`victris_engine::turnfeat`) of named bots from replays.
//!
//!   turnfit <replay dirs> --bots A,B,... [--base generic.json] [--pull 1.0] [--epochs 8]
//!           [--maps columns-21] [--out model.json] [--name NAME]
//!
//! One example per tick on which a named bot could act: the turns the search would consider for
//! it, and the one it played (ticks where that turn is not in the list are skipped). Fitted by
//! Adam on the softmax log-likelihood, with an L2 pull of strength `--pull` toward the `--base`
//! weights (toward zero without a base). Every fifth game is held out. Reports, on held-out
//! ticks, the mean probability and log-probability of the turn actually played and how often it
//! was the most likely, for the fitted model, the base model, and the search's current priors.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::mcts::actions_with;
use victris_engine::replay::parse_replay;
use victris_engine::state::FState;
use victris_engine::turnfeat::{turn_features, TurnModel, CLASS_BANDS, SHAPED_NAMES, TF};

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

/// One candidate turn, sparse: its class-band index and the shaped features.
#[derive(Clone, Copy)]
struct Cand {
    cb: u16,
    shaped: [f32; TF - CLASS_BANDS],
}

struct Example {
    test: bool,
    cands: Vec<Cand>,
    actual: usize,
    prior_actual: f32, // the search's current prior of the actual turn
    prior_top: bool,   // was it the search's most likely turn
}

fn score(w: &[f64], c: &Cand) -> f64 {
    let mut z = w[c.cb as usize];
    for (i, v) in c.shaped.iter().enumerate() {
        z += w[CLASS_BANDS + i] * *v as f64;
    }
    z
}

fn probs(w: &[f64], ex: &Example) -> Vec<f64> {
    let z: Vec<f64> = ex.cands.iter().map(|c| score(w, c)).collect();
    let m = z.iter().cloned().fold(f64::MIN, f64::max);
    let e: Vec<f64> = z.iter().map(|v| (v - m).exp()).collect();
    let s: f64 = e.iter().sum();
    e.iter().map(|v| v / s).collect()
}

fn eval(w: &[f64], exs: &[Example]) -> (f64, f64, f64, usize) {
    let (mut p, mut lp, mut top, mut n) = (0.0, 0.0, 0.0, 0usize);
    for ex in exs.iter().filter(|e| e.test) {
        let pr = probs(w, ex);
        let pa = pr[ex.actual];
        p += pa;
        lp += pa.max(1e-9).ln();
        let best = pr.iter().cloned().fold(f64::MIN, f64::max);
        top += (pa >= best) as usize as f64;
        n += 1;
    }
    let nn = n.max(1) as f64;
    (p / nn, lp / nn, top / nn, n)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).expect("--bots A,B");
    let maps: Vec<String> = opt("--maps").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let epochs: usize = opt("--epochs").and_then(|s| s.parse().ok()).unwrap_or(8);
    let pull: f64 = opt("--pull").and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let base: Option<TurnModel> = opt("--base").and_then(|p| TurnModel::load(Path::new(&p)));
    let out = opt("--out");
    let name = opt("--name").unwrap_or_else(|| bots.join(","));
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if ["--bots", "--maps", "--epochs", "--pull", "--base", "--out", "--name"].contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let mut exs: Vec<Example> = Vec::new();
    let (mut games, mut skipped) = (0usize, 0usize);
    for path in &files {
        let mut text = String::new();
        if path.to_string_lossy().ends_with(".gz") {
            if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else {
            text = std::fs::read_to_string(path).unwrap_or_default();
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        if !maps.is_empty() && !maps.contains(&e.map.name) {
            continue;
        }
        let sides: Vec<usize> = (0..2).filter(|x| bots.contains(&replay.bots[*x])).collect();
        if sides.is_empty() {
            continue;
        }
        let test = games % 5 == 4;
        games += 1;
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            for &x in &sides {
                let f = &s.fighters[x];
                if f.state != FState::Idle || !f.alive || !s.fighters[1 - x].alive {
                    continue;
                }
                let list = actions_with(e, &s, x, 0.25, false, false, false, true);
                let Some(actual) = list.iter().position(|a| a.turn == t.turns[x]) else {
                    skipped += 1;
                    continue;
                };
                let tot: f32 = list.iter().map(|a| a.prior).sum();
                let best = list.iter().map(|a| a.prior).fold(f32::MIN, f32::max);
                let cands: Vec<Cand> = list
                    .iter()
                    .map(|a| {
                        let ft = turn_features(e, &s, x, &a.turn);
                        let cb = ft[..CLASS_BANDS].iter().position(|v| *v > 0.5).unwrap_or(0) as u16;
                        let mut shaped = [0.0f32; TF - CLASS_BANDS];
                        shaped.copy_from_slice(&ft[CLASS_BANDS..]);
                        Cand { cb, shaped }
                    })
                    .collect();
                exs.push(Example { test, cands, actual, prior_actual: list[actual].prior / tot.max(1e-9), prior_top: list[actual].prior >= best });
            }
            e.step(&mut s, &t.turns);
        }
    }
    let b: Vec<f64> = base.as_ref().map(|m| m.w.iter().map(|v| *v as f64).collect()).unwrap_or_else(|| vec![0.0; TF]);
    let mut w = b.clone();
    let (mut m1, mut m2) = (vec![0.0f64; TF], vec![0.0f64; TF]);
    let (lr, b1, b2) = (0.02, 0.9, 0.999);
    let train: Vec<usize> = (0..exs.len()).filter(|i| !exs[*i].test).collect();
    let ntrain = train.len().max(1) as f64;
    let mut step = 0usize;
    let mut rng = 0x9e3779b97f4a7c15u64;
    for _ in 0..epochs {
        let mut order = train.clone();
        for i in (1..order.len()).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            order.swap(i, (rng % (i as u64 + 1)) as usize);
        }
        for chunk in order.chunks(256) {
            let mut g = vec![0.0f64; TF];
            for &i in chunk {
                let ex = &exs[i];
                let pr = probs(&w, ex);
                for (j, c) in ex.cands.iter().enumerate() {
                    let coef = pr[j] - if j == ex.actual { 1.0 } else { 0.0 };
                    g[c.cb as usize] += coef;
                    for (k, v) in c.shaped.iter().enumerate() {
                        g[CLASS_BANDS + k] += coef * *v as f64;
                    }
                }
            }
            step += 1;
            for k in 0..TF {
                let gk = g[k] / chunk.len() as f64 + pull * (w[k] - b[k]) / ntrain * 100.0;
                m1[k] = b1 * m1[k] + (1.0 - b1) * gk;
                m2[k] = b2 * m2[k] + (1.0 - b2) * gk * gk;
                let mh = m1[k] / (1.0 - b1.powi(step as i32));
                let vh = m2[k] / (1.0 - b2.powi(step as i32));
                w[k] -= lr * mh / (vh.sqrt() + 1e-8);
            }
        }
    }
    let (p, lp, top, n) = eval(&w, &exs);
    let (bp, blp, btop, _) = eval(&b, &exs);
    let test: Vec<&Example> = exs.iter().filter(|e| e.test).collect();
    let nn = test.len().max(1) as f64;
    let sp = test.iter().map(|e| e.prior_actual as f64).sum::<f64>() / nn;
    let slp = test.iter().map(|e| (e.prior_actual as f64).max(1e-9).ln()).sum::<f64>() / nn;
    let stop = test.iter().filter(|e| e.prior_top).count() as f64 / nn;
    println!("{} games, {} decisions ({} held out), {} skipped (played turn not in the list)", games, exs.len(), n, skipped);
    println!("  the turn actually played, on held-out ticks: mean probability / mean log-probability / most likely");
    println!("    fitted model  {:.3} / {:.3} / {:.1}%", p, lp, 100.0 * top);
    println!("    {}  {:.3} / {:.3} / {:.1}%", if base.is_some() { "base model  " } else { "uniform     " }, bp, blp, 100.0 * btop);
    println!("    search priors {:.3} / {:.3} / {:.1}%", sp, slp, 100.0 * stop);
    println!("  shaped weights: {}", SHAPED_NAMES.iter().enumerate().map(|(i, nm)| format!("{nm} {:+.2}", w[CLASS_BANDS + i])).collect::<Vec<_>>().join(", "));
    if let Some(o) = out {
        let json = serde_json::json!({ "name": name, "bots": bots, "games": games, "decisions": exs.len(), "heldout_mean_p": p, "heldout_logp": lp, "w": w.iter().map(|v| *v as f32).collect::<Vec<f32>>() });
        std::fs::write(&o, serde_json::to_string(&json).unwrap()).expect("write");
        println!("  wrote {o}");
    }
}
