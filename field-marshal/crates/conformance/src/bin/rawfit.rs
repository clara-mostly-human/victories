//! rawfit: trains the raw-board judgment (`victris_engine::rawnet`) of one map from replays.
//!
//!   rawfit <replay dirs> --map columns-21 [--h1 64] [--h2 32] [--epochs 10] [--every 3]
//!          [--init net.json] [--out net.json] [--threads 6]
//!
//! Every `--every`-th tick of every game on the map (both fighters alive, tick >= 5): the board,
//! and the discounted hp fighter 0 went on to gain or lose (0.98 a tick, 40 for a kill), the
//! same target the linear judgment is fitted to. Every fifth game is held out. Trained by Adam on
//! squared error. Reports, on the held-out positions, how much of the outcome the network explains
//! next to the linear judgment row 13 (the flagship's).

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::rawnet::{act, encode, RawNet, IN};
use victris_engine::replay::parse_replay;

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

/// One position: both encodings quantised to i8 (inputs lie in [-1, 1]), the target, the linear judgment.
struct Sample {
    a: [i8; IN],
    b: [i8; IN],
    y: f32,
    lin: f32,
    test: bool,
}

fn q(x: &[f32; IN]) -> [i8; IN] {
    let mut o = [0i8; IN];
    for i in 0..IN {
        o[i] = (x[i].clamp(-1.0, 1.0) * 127.0).round() as i8;
    }
    o
}

fn dq(x: &[i8; IN]) -> [f32; IN] {
    let mut o = [0.0f32; IN];
    for i in 0..IN {
        o[i] = x[i] as f32 / 127.0;
    }
    o
}

/// Forward pass keeping the activations; returns g.
fn forward(net: &RawNet, x: &[f32; IN], a1: &mut [f32], a2: &mut [f32]) -> f32 {
    let (h1, h2) = (net.h1, net.h2);
    let w = &net.w;
    for j in 0..h1 {
        let row = &w[j * IN..(j + 1) * IN];
        let mut z = w[h1 * IN + j];
        for i in 0..IN {
            z += row[i] * x[i];
        }
        a1[j] = act(z);
    }
    let b2 = h1 * IN + h1;
    let mut out = w[b2 + h2 * h1 + h2 + h2];
    for j in 0..h2 {
        let row = &w[b2 + j * h1..b2 + (j + 1) * h1];
        let mut z = w[b2 + h2 * h1 + j];
        for i in 0..h1 {
            z += row[i] * a1[i];
        }
        a2[j] = act(z);
        out += w[b2 + h2 * h1 + h2 + j] * a2[j];
    }
    out
}

/// Adds coef * d g(x) / d w to `g`.
fn backward(net: &RawNet, x: &[f32; IN], a1: &[f32], a2: &[f32], coef: f32, g: &mut [f32]) {
    let (h1, h2) = (net.h1, net.h2);
    let w = &net.w;
    let b2 = h1 * IN + h1;
    let ow = b2 + h2 * h1 + h2;
    g[ow + h2] += coef;
    let mut d1 = vec![0.0f32; h1];
    for j in 0..h2 {
        g[ow + j] += coef * a2[j];
        let dz = coef * w[ow + j] * (1.0 - a2[j] * a2[j]);
        g[b2 + h2 * h1 + j] += dz;
        for i in 0..h1 {
            g[b2 + j * h1 + i] += dz * a1[i];
            d1[i] += dz * w[b2 + j * h1 + i];
        }
    }
    for j in 0..h1 {
        let dz = d1[j] * (1.0 - a1[j] * a1[j]);
        if dz == 0.0 {
            continue;
        }
        g[h1 * IN + j] += dz;
        let row = &mut g[j * IN..(j + 1) * IN];
        for i in 0..IN {
            row[i] += dz * x[i];
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let map = opt("--map").unwrap_or_else(|| "columns-21".into());
    let h1: usize = opt("--h1").and_then(|s| s.parse().ok()).unwrap_or(64);
    let h2: usize = opt("--h2").and_then(|s| s.parse().ok()).unwrap_or(32);
    let epochs: usize = opt("--epochs").and_then(|s| s.parse().ok()).unwrap_or(10);
    let every: usize = opt("--every").and_then(|s| s.parse().ok()).unwrap_or(3);
    let threads: usize = opt("--threads").and_then(|s| s.parse().ok()).unwrap_or(6);
    let init = opt("--init").and_then(|p| RawNet::load(Path::new(&p)));
    let out = opt("--out");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if ["--map", "--h1", "--h2", "--epochs", "--every", "--init", "--out", "--threads"].contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let lin_w = victris_engine::models::value_weights(13);
    let mut samples: Vec<Sample> = Vec::new();
    let mut games = 0usize;
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
        if e.map.name != map {
            continue;
        }
        let test = games % 5 == 4;
        games += 1;
        let mut s = e.new_match(replay.seed);
        let mut rec: Vec<(usize, [i8; IN], [i8; IN], f32)> = Vec::new();
        let mut reward: Vec<f64> = Vec::new();
        let (mut ea, mut eb) = ([0.0f32; IN], [0.0f32; IN]);
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            if ti % every == 0 && s.tick >= 5 && s.fighters[0].alive && s.fighters[1].alive {
                encode(e, &s, 0, &mut ea);
                encode(e, &s, 1, &mut eb);
                let lin = victris_engine::valuefeat::value(e, &s, 0, lin_w);
                rec.push((ti, q(&ea), q(&eb), lin));
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
        for (ti, a, b, lin) in rec {
            samples.push(Sample { a, b, y: to_come[ti] as f32, lin, test });
        }
    }
    let n_params = RawNet::params(h1, h2);
    let mut net = match init {
        Some(n) if n.h1 == h1 && n.h2 == h2 => n,
        _ => {
            let mut rng = 0x2545f4914f6cdd1du64;
            let mut w = vec![0.0f32; n_params];
            for v in w.iter_mut() {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *v = ((rng % 20001) as f32 / 10000.0 - 1.0) * 0.1;
            }
            RawNet { map: map.clone(), h1, h2, w }
        }
    };
    let train: Vec<usize> = (0..samples.len()).filter(|i| !samples[*i].test).collect();
    let test: Vec<usize> = (0..samples.len()).filter(|i| samples[*i].test).collect();
    eprintln!("{} games on {map}, {} positions ({} held out), {} weights", games, samples.len(), test.len(), n_params);
    let eval = |net: &RawNet| -> (f64, f64, f64) {
        let (mut sse, mut sst, mut sse_lin) = (0.0f64, 0.0f64, 0.0f64);
        let mean: f64 = test.iter().map(|i| samples[*i].y as f64).sum::<f64>() / test.len().max(1) as f64;
        let (mut a1, mut a2) = (vec![0.0f32; h1], vec![0.0f32; h2]);
        for &i in &test {
            let sm = &samples[i];
            let p = forward(net, &dq(&sm.a), &mut a1, &mut a2) - forward(net, &dq(&sm.b), &mut a1, &mut a2);
            sse += (p as f64 - sm.y as f64).powi(2);
            sse_lin += (sm.lin as f64 - sm.y as f64).powi(2);
            sst += (sm.y as f64 - mean).powi(2);
        }
        (1.0 - sse / sst.max(1e-9), 1.0 - sse_lin / sst.max(1e-9), (sse / test.len().max(1) as f64).sqrt())
    };
    let (mut m1, mut m2) = (vec![0.0f32; n_params], vec![0.0f32; n_params]);
    let (lr, be1, be2) = (0.002f32, 0.9f32, 0.999f32);
    let mut step = 0i32;
    let mut rng = 0x9e3779b97f4a7c15u64;
    for ep in 0..epochs {
        let mut order = train.clone();
        for i in (1..order.len()).rev() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            order.swap(i, (rng % (i as u64 + 1)) as usize);
        }
        for chunk in order.chunks(512) {
            let per = chunk.len().div_ceil(threads.max(1));
            let grads: Vec<Vec<f32>> = std::thread::scope(|sc| {
                let hs: Vec<_> = chunk
                    .chunks(per.max(1))
                    .map(|part| {
                        let net = &net;
                        let samples = &samples;
                        sc.spawn(move || {
                            let mut g = vec![0.0f32; n_params];
                            let (mut a1, mut a2, mut b1, mut b2v) = (vec![0.0f32; h1], vec![0.0f32; h2], vec![0.0f32; h1], vec![0.0f32; h2]);
                            for &i in part {
                                let sm = &samples[i];
                                let (xa, xb) = (dq(&sm.a), dq(&sm.b));
                                let p = forward(net, &xa, &mut a1, &mut a2) - forward(net, &xb, &mut b1, &mut b2v);
                                let err = (p - sm.y) / 20.0; // targets are tens of hp
                                backward(net, &xa, &a1, &a2, err, &mut g);
                                backward(net, &xb, &b1, &b2v, -err, &mut g);
                            }
                            g
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            step += 1;
            let nb = chunk.len() as f32;
            for k in 0..n_params {
                let gk: f32 = grads.iter().map(|g| g[k]).sum::<f32>() / nb + 1e-5 * net.w[k];
                m1[k] = be1 * m1[k] + (1.0 - be1) * gk;
                m2[k] = be2 * m2[k] + (1.0 - be2) * gk * gk;
                let mh = m1[k] / (1.0 - be1.powi(step));
                let vh = m2[k] / (1.0 - be2.powi(step));
                net.w[k] -= lr * mh / (vh.sqrt() + 1e-8);
            }
        }
        let (r2, r2_lin, rmse) = eval(&net);
        eprintln!("epoch {}: held-out explained {:.1}% (linear row 13: {:.1}%), typical miss {:.1} hp", ep + 1, 100.0 * r2, 100.0 * r2_lin, rmse);
    }
    let (r2, r2_lin, _) = eval(&net);
    println!("{map}: {} games, {} positions; held-out explained: network {:.1}%, linear row 13 {:.1}%", games, samples.len(), 100.0 * r2, 100.0 * r2_lin);
    if let Some(o) = out {
        let json = serde_json::json!({ "map": map, "h1": h1, "h2": h2, "in": IN, "games": games, "heldout_r2": r2, "w": net.w });
        std::fs::write(&o, serde_json::to_string(&json).unwrap()).expect("write");
        println!("wrote {o}");
    }
}
