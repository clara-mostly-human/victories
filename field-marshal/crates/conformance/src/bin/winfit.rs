//! winfit: trains the win-probability judgment (`victris_engine::valuenet`) on replays.
//!
//!   winfit --bots A,B,... [--epochs 12] [--every 3] [--from 20] [--lr 0.001]
//!          [--emit path.rs] <replay dir>...
//!
//! Every `--every`-th tick from tick `--from` of every game whose two bots are both in
//! `--bots` becomes one example: the network's inputs for fighter 0 and fighter 1 and the
//! position, labelled with whether fighter 0 went on to win (draws are left out). The model
//! is the antisymmetric network of `valuenet`, fitted by Adam on the log loss. Every fifth
//! game is held out; the tool reports held-out log loss and accuracy for the network, for
//! hp alone (one logistic weight on the hp difference) and for a coin. With --emit it
//! writes the weights as a Rust file for the engine.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::replay::parse_replay;
use victris_engine::valuenet::{ctx, g, own, CTX, H1, H2, IN, NW, OWN};

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

struct Ex {
    a: [f32; OWN],
    b: [f32; OWN],
    c: [f32; CTX],
    hp: f32,
    y: f32,
}

/// Adds the gradient of the output of `g(mine, theirs, c)` times `scale` to `grad`.
fn g_grad(w: &[f32], mine: &[f32; OWN], theirs: &[f32; OWN], c: &[f32; CTX], scale: f32, grad: &mut [f32]) {
    let mut input = [0.0f32; IN];
    input[..OWN].copy_from_slice(mine);
    input[OWN..2 * OWN].copy_from_slice(theirs);
    input[2 * OWN..].copy_from_slice(c);
    let o_b1 = IN * H1;
    let o_w2 = o_b1 + H1;
    let o_b2 = o_w2 + H1 * H2;
    let o_w3 = o_b2 + H2;
    let o_b3 = o_w3 + H2;
    let mut z1 = [0.0f32; H1];
    let mut h1 = [0.0f32; H1];
    for j in 0..H1 {
        let mut z = w[o_b1 + j];
        for i in 0..IN {
            z += w[j * IN + i] * input[i];
        }
        z1[j] = z;
        h1[j] = z.max(0.0);
    }
    let mut z2 = [0.0f32; H2];
    let mut h2 = [0.0f32; H2];
    for j in 0..H2 {
        let mut z = w[o_b2 + j];
        for i in 0..H1 {
            z += w[o_w2 + j * H1 + i] * h1[i];
        }
        z2[j] = z;
        h2[j] = z.max(0.0);
    }
    // output = b3 + w3 . h2
    grad[o_b3] += scale;
    let mut d2 = [0.0f32; H2];
    for j in 0..H2 {
        grad[o_w3 + j] += scale * h2[j];
        d2[j] = if z2[j] > 0.0 { scale * w[o_w3 + j] } else { 0.0 };
    }
    let mut d1 = [0.0f32; H1];
    for j in 0..H2 {
        if d2[j] == 0.0 {
            continue;
        }
        grad[o_b2 + j] += d2[j];
        for i in 0..H1 {
            grad[o_w2 + j * H1 + i] += d2[j] * h1[i];
            d1[i] += d2[j] * w[o_w2 + j * H1 + i];
        }
    }
    for j in 0..H1 {
        if z1[j] <= 0.0 || d1[j] == 0.0 {
            continue;
        }
        grad[o_b1 + j] += d1[j];
        for i in 0..IN {
            grad[j * IN + i] += d1[j] * input[i];
        }
    }
}

fn prob(w: &[f32], x: &Ex) -> f32 {
    let z = g(w, &x.a, &x.b, &x.c) - g(w, &x.b, &x.a, &x.c);
    1.0 / (1.0 + (-z).exp())
}

fn logloss(p: f32, y: f32) -> f64 {
    let p = p.clamp(1e-6, 1.0 - 1e-6) as f64;
    -(y as f64 * p.ln() + (1.0 - y as f64) * (1.0 - p).ln())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let epochs: usize = opt("--epochs").and_then(|s| s.parse().ok()).unwrap_or(12);
    let every: usize = opt("--every").and_then(|s| s.parse().ok()).unwrap_or(3).max(1);
    let from: i32 = opt("--from").and_then(|s| s.parse().ok()).unwrap_or(20);
    let lr: f32 = opt("--lr").and_then(|s| s.parse().ok()).unwrap_or(0.001);
    let emit = opt("--emit");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if ["--bots", "--epochs", "--every", "--from", "--lr", "--emit"].contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let (mut train, mut test): (Vec<Ex>, Vec<Ex>) = (Vec::new(), Vec::new());
    let mut games = 0usize;
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        if !bots.is_empty() && !replay.bots.iter().all(|n| bots.contains(n)) {
            continue;
        }
        let e = &replay.engine;
        // the winner, by playing the game out (the result line may be missing)
        let mut s = e.new_match(replay.seed);
        let mut samples: Vec<([f32; OWN], [f32; OWN], [f32; CTX], f32)> = Vec::new();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            if ti % every == 0 && s.tick >= from && s.fighters[0].alive && s.fighters[1].alive {
                let hp = (s.fighters[0].hp - s.fighters[1].hp) as f32 / e.rules.max_hp as f32;
                samples.push((own(e, &s, 0), own(e, &s, 1), ctx(e, &s), hp));
            }
            e.step(&mut s, &t.turns);
        }
        let Some(r) = s.result else { continue };
        let Some(winner) = r.winner else { continue };
        games += 1;
        let y = if winner == 1 { 1.0 } else { 0.0 };
        let held = games % 5 == 0;
        for (a, b, c, hp) in samples {
            let ex = Ex { a, b, c, hp, y };
            if held { test.push(ex) } else { train.push(ex) }
        }
    }
    println!("{games} games; {} training positions, {} held out", train.len(), test.len());
    if train.is_empty() {
        return;
    }
    // hp alone: one weight, fitted by a few passes of gradient descent
    let mut k = 0.0f64;
    for _ in 0..200 {
        let mut gsum = 0.0;
        for x in &train {
            let p = 1.0 / (1.0 + (-(k * x.hp as f64)).exp());
            gsum += (p - x.y as f64) * x.hp as f64;
        }
        k -= 2.0 * gsum / train.len() as f64;
    }
    // the network
    let mut seed = 0x2545f4914f6cdd1du64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 40) as f32) / ((1u64 << 24) as f32) - 0.5
    };
    let mut w = vec![0.0f32; NW];
    for (i, v) in w.iter_mut().enumerate() {
        let fan = if i < IN * H1 { IN } else if i < IN * H1 + H1 + H1 * H2 { H1 } else { H2 };
        *v = rnd() * 2.0 * (2.0 / fan as f32).sqrt();
    }
    let (mut m, mut v2) = (vec![0.0f32; NW], vec![0.0f32; NW]);
    let mut order: Vec<usize> = (0..train.len()).collect();
    let mut step = 0i32;
    let eval = |w: &[f32], rows: &[Ex]| -> (f64, f64) {
        let (mut ll, mut acc) = (0.0, 0.0);
        for x in rows {
            let p = prob(w, x);
            ll += logloss(p, x.y);
            acc += ((p > 0.5) == (x.y > 0.5)) as i32 as f64;
        }
        (ll / rows.len().max(1) as f64, acc / rows.len().max(1) as f64)
    };
    let mut best = (f64::MAX, w.clone());
    for epoch in 0..epochs {
        for i in (1..order.len()).rev() {
            let j = ((rnd() + 0.5) * (i + 1) as f32) as usize % (i + 1);
            order.swap(i, j);
        }
        for batch in order.chunks(256) {
            let mut grad = vec![0.0f32; NW];
            for &i in batch {
                let x = &train[i];
                let d = prob(&w, x) - x.y;
                g_grad(&w, &x.a, &x.b, &x.c, d, &mut grad);
                g_grad(&w, &x.b, &x.a, &x.c, -d, &mut grad);
            }
            step += 1;
            let n = batch.len() as f32;
            for k in 0..NW {
                let gk = grad[k] / n + 1e-5 * w[k];
                m[k] = 0.9 * m[k] + 0.1 * gk;
                v2[k] = 0.999 * v2[k] + 0.001 * gk * gk;
                let mh = m[k] / (1.0 - 0.9f32.powi(step));
                let vh = v2[k] / (1.0 - 0.999f32.powi(step));
                w[k] -= lr * mh / (vh.sqrt() + 1e-8);
            }
        }
        let (ll, acc) = eval(&w, &test);
        println!("  epoch {:>2}: held-out log loss {:.4}, accuracy {:.1}%", epoch + 1, ll, 100.0 * acc);
        if ll < best.0 {
            best = (ll, w.clone());
        }
    }
    let w = best.1;
    let (ll, acc) = eval(&w, &test);
    let (mut hl, mut ha) = (0.0, 0.0);
    for x in &test {
        let p = (1.0 / (1.0 + (-(k * x.hp as f64)).exp())) as f32;
        hl += logloss(p, x.y);
        ha += ((p > 0.5) == (x.y > 0.5)) as i32 as f64;
    }
    let n = test.len().max(1) as f64;
    println!("held out: network log loss {:.4} accuracy {:.1}%; hp alone {:.4} {:.1}%; a coin 0.6931 50%", ll, 100.0 * acc, hl / n, 100.0 * ha / n);
    if let Some(path) = emit {
        let body = w.iter().map(|v| format!("{v:.5}")).collect::<Vec<_>>().join(", ");
        let text = format!(
            "// Written by `winfit` from {games} games ({} positions); held-out log loss {:.4} (hp alone {:.4}).\npub const VALUE_NET: [f32; crate::valuenet::NW] = [{body}];\n",
            train.len() + test.len(),
            ll,
            hl / n
        );
        std::fs::write(&path, text).expect("write the weights");
        println!("weights written to {path}");
    }
}
