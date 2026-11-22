//! aztrain: teaches the network (`pvnet`) from self-play files written by `azgen`.
//!
//! The value learns who went on to win (mixed with the search's own value where it has one);
//! the turn shares learn where the search put its visits.
//!
//!   aztrain --out NET.json --data FILE [FILE ...] [--init NET.json] [--h1 64] [--h2 32]
//!           [--epochs 8] [--lr 0.002] [--batch 512] [--lambda 0.5] [--pw 1.0] [--threads 4]
//!           [--holdout 0.05] [--seed 1] [--map columns-21] [--mirror 1] [--widen]
//!           [--beside 1]
//!
//! Games saved with positions beside their path (`azgen --side`) teach the value of those
//! positions too: the search's value of each, and no turns. --beside 0 leaves them out.
//!
//! With --widen an --init network made before the walls three tiles from the opponent were
//! inputs (`pvnet::IN_OLD`) is given them (`PvNet::widened`); a new network always has them.
//!
//! With --mirror (on by default) every position is shown in a mirror image of the map drawn
//! afresh each time (`pvnet::Sym`), where the map mirrors onto itself.

use victris_engine::azdata::GameLog;
use victris_engine::pvnet::{loss_grad, Loss, PvNet, Sample, Scratch, Sym};
use victris_engine::pvsearch::Rng;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[derive(Default, Clone, Copy)]
struct Sum {
    n: f64,
    value: f64,
    decided: f64,
    hit: f64,
    policy: f64,
    floor: f64,
    pn: f64,
    top: f64,
    /// positions beside the games' paths, and their value loss (kept out of `n` and `value`)
    side_n: f64,
    side_value: f64,
}

impl Sum {
    fn add(&mut self, l: &Loss) {
        if l.side {
            self.side_n += 1.0;
            self.side_value += l.value as f64;
            return;
        }
        self.n += 1.0;
        self.value += l.value as f64;
        self.decided += l.decided as u8 as f64;
        self.hit += (l.decided && l.value_hit) as u8 as f64;
        self.policy += l.policy as f64;
        self.floor += l.policy_floor as f64;
        self.pn += l.policy_n as f64;
        self.top += l.top_hit as f64;
    }
    fn merge(&mut self, o: &Sum) {
        self.n += o.n;
        self.value += o.value;
        self.decided += o.decided;
        self.hit += o.hit;
        self.policy += o.policy;
        self.floor += o.floor;
        self.pn += o.pn;
        self.top += o.top;
        self.side_n += o.side_n;
        self.side_value += o.side_value;
    }
    fn line(&self) -> String {
        let beside = if self.side_n > 0.0 { format!("; beside the games ({:.0} positions) value loss {:.4}", self.side_n, self.side_value / self.side_n) } else { String::new() };
        format!(
            "value loss {:.4}, picks the winner {:.1}%; moves loss {:.4} (the search's own spread {:.4}), same first choice as the search {:.1}%{beside}",
            self.value / self.n.max(1.0),
            100.0 * self.hit / self.decided.max(1.0),
            self.policy / self.pn.max(1.0),
            self.floor / self.pn.max(1.0),
            100.0 * self.top / self.pn.max(1.0)
        )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out: String = arg(&args, "--out", String::from("net.json"));
    let init: String = arg(&args, "--init", String::new());
    let widen = args.iter().any(|a| a == "--widen");
    let beside = arg::<u32>(&args, "--beside", 1) != 0;
    let (h1, h2): (usize, usize) = (arg(&args, "--h1", 64), arg(&args, "--h2", 32));
    let epochs: usize = arg(&args, "--epochs", 8);
    let lr0: f32 = arg(&args, "--lr", 0.002);
    let batch: usize = arg(&args, "--batch", 512);
    let lambda: f32 = arg(&args, "--lambda", 0.5);
    let pw: f32 = arg(&args, "--pw", 1.0);
    let eps: f32 = arg(&args, "--eps", 1e-3);
    let threads: usize = arg(&args, "--threads", 4);
    let holdout: f64 = arg(&args, "--holdout", 0.05);
    let seed: u64 = arg(&args, "--seed", 1);
    let map: String = arg(&args, "--map", String::from("columns-21"));
    let mirror: u32 = arg(&args, "--mirror", 1);
    let e = victris_engine::Engine::shipped(&map);
    let sym = Sym::of(&e);
    let images: Vec<usize> = (0..4).filter(|k| *k == 0 || (mirror != 0 && sym.ok[*k])).collect();
    let files: Vec<String> = match args.iter().position(|a| a == "--data") {
        Some(i) => args[i + 1..].iter().take_while(|a| !a.starts_with("--")).cloned().collect(),
        None => Vec::new(),
    };
    // replay every saved game to get its positions back
    let mut data: Vec<Sample> = Vec::new();
    let mut game_start: Vec<usize> = Vec::new();
    let mut bad = 0;
    for f in &files {
        let b = std::fs::read(f).unwrap_or_else(|_| panic!("cannot read {f}"));
        for g in GameLog::read_all(&b) {
            let at = data.len();
            if g.samples(&e, &mut data) {
                if !beside {
                    // the game's own positions only (they are first in line, so `at` stands)
                    let mut k = at;
                    for i in at..data.len() {
                        if !data[i].side {
                            data.swap(k, i);
                            k += 1;
                        }
                    }
                    data.truncate(k);
                }
                game_start.push(at);
            } else {
                bad += 1;
            }
        }
    }
    let total = data.len();
    assert!(total > 0, "no positions to learn from");
    let mut rng = Rng::new(seed);
    // hold out whole games, so the check is on games the network did not train on
    let mut train: Vec<u32> = Vec::new();
    let mut held: Vec<u32> = Vec::new();
    for (k, &at) in game_start.iter().enumerate() {
        let end = game_start.get(k + 1).copied().unwrap_or(total);
        let to = if rng.unit() < holdout { &mut held } else { &mut train };
        to.extend((at..end).map(|i| i as u32));
    }
    let mut net = if init.is_empty() { PvNet::blank(&map, h1, h2, seed) } else { PvNet::load(std::path::Path::new(&init)).expect("cannot read --init") };
    // `--widen`: a network made before the newest inputs gets them here, with weights of zero,
    // so it starts from what it knows and learns them on top. Without it the network keeps
    // the inputs it has, and stays readable by the programs that were built before them.
    if widen {
        net = net.widened();
    }
    let len = net.layout().len;
    if init.is_empty() {
        // Start every kind of turn at its overall share of the search's visits. Left at zero,
        // the rare kinds need thousands of steps to sink, and all that time every weight is
        // pushed the same way: the hidden units end up pinned at their limits acting as one
        // more constant, and learn nothing about the position.
        let mut share = vec![1e-4f64; victris_engine::pvnet::CLASSES];
        for r in data.iter().step_by(7) {
            for f in 0..2 {
                let sum: u32 = r.pol[f].iter().map(|v| *v as u32).sum();
                if sum > 0 {
                    for c in 0..share.len() {
                        share[c] += r.pol[f][c] as f64 / sum as f64;
                    }
                }
            }
        }
        let all: f64 = share.iter().sum();
        let pb = net.layout().pb;
        for c in 0..share.len() {
            net.w[pb + c] = (share[c] / all).ln() as f32;
        }
    }
    println!("aztrain: {} positions ({} held out) from {} games in {} files{}, network {}x{} ({} weights), {} epochs, {} mirror images", total, held.len(), game_start.len(), files.len(), if bad > 0 { format!(" ({bad} games did not replay and were left out)") } else { String::new() }, net.h1, net.h2, len, epochs, images.len());
    let (data_ref, e_ref) = (&data, &e);
    let rec = move |i: u32| &data_ref[i as usize];
    let check = |net: &PvNet, ids: &[u32]| -> Sum {
        let chunk = ids.len().div_ceil(threads.max(1)).max(1);
        std::thread::scope(|sc| {
            let hs: Vec<_> = ids.chunks(chunk).map(|part| sc.spawn(move || {
                let (mut sum, mut scr) = (Sum::default(), Scratch::default());
                for &i in part {
                    sum.add(&loss_grad(net, e_ref, rec(i), lambda, pw, &mut scr, None));
                }
                sum
            })).collect();
            let mut all = Sum::default();
            for h in hs {
                all.merge(&h.join().unwrap());
            }
            all
        })
    };
    if !held.is_empty() {
        println!("  before: held out: {}", check(&net, &held).line());
    }
    let (mut m, mut v) = (vec![0.0f32; len], vec![0.0f32; len]);
    let (b1, b2) = (0.9f32, 0.999f32);
    let mut step = 0i32;
    let started = std::time::Instant::now();
    for epoch in 0..epochs {
        // shuffle
        for i in (1..train.len()).rev() {
            train.swap(i, rng.below(i + 1));
        }
        let lr = if epoch * 3 >= epochs * 2 { lr0 * 0.3 } else { lr0 };
        let mut seen = Sum::default();
        for ids in train.chunks(batch) {
            let chunk = ids.len().div_ceil(threads.max(1)).max(1);
            let (net_ref, sym_ref, images_ref) = (&net, &sym, &images);
            let draw = rng.next();
            let parts: Vec<(Vec<f32>, Sum)> = std::thread::scope(|sc| {
                let hs: Vec<_> = ids.chunks(chunk).map(|part| sc.spawn(move || {
                    let mut grad = vec![0.0f32; len];
                    let (mut sum, mut scr) = (Sum::default(), Scratch::default());
                    for &i in part {
                        let k = images_ref[((draw ^ (i as u64).wrapping_mul(0x9E3779B97F4A7C15)) >> 33) as usize % images_ref.len()];
                        let l = if k == 0 { loss_grad(net_ref, e_ref, rec(i), lambda, pw, &mut scr, Some(&mut grad)) } else { loss_grad(net_ref, e_ref, &sym_ref.image(k, rec(i)), lambda, pw, &mut scr, Some(&mut grad)) };
                        sum.add(&l);
                    }
                    (grad, sum)
                })).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            step += 1;
            let scale = 1.0 / ids.len() as f32;
            let (c1, c2) = (1.0 - b1.powi(step), 1.0 - b2.powi(step));
            for k in 0..len {
                let g: f32 = parts.iter().map(|p| p.0[k]).sum::<f32>() * scale;
                m[k] = b1 * m[k] + (1.0 - b1) * g;
                v[k] = b2 * v[k] + (1.0 - b2) * g * g;
                net.w[k] -= lr * (m[k] / c1) / ((v[k] / c2).sqrt() + eps);
            }
            for p in &parts {
                seen.merge(&p.1);
            }
        }
        if std::env::var("AZ_DEBUG").is_ok() {
            let l = net.layout();
            let norm = |a: usize, b: usize| (net.w[a..b].iter().map(|x| (*x as f64).powi(2)).sum::<f64>() / (b - a).max(1) as f64).sqrt();
            let (mut sat1, mut sat2, mut n1, mut n2) = (0u64, 0u64, 0u64, 0u64);
            let mut pass = victris_engine::pvnet::Pass::default();
            let mut x = victris_engine::pvnet::Inputs::default();
            for &i in train.iter().take(2000) {
                victris_engine::pvnet::encode(&e, &rec(i).s, 0, &mut x);
                net.forward(&x, &mut pass);
                for j in 0..net.h1 {
                    sat1 += (pass.z1[j].abs() > 4.0) as u64;
                    n1 += 1;
                }
                for j in 0..net.h2 {
                    sat2 += (pass.z2[j].abs() > 4.0) as u64;
                    n2 += 1;
                }
            }
            println!("      debug: rms weights t1 {:.3} t2 {:.3} vw {:.3} tp {:.3}; units past 4: layer 1 {:.1}%, layer 2 {:.1}%", norm(l.t1, l.b1), norm(l.t2, l.b2), norm(l.vw, l.vb), norm(l.tp, l.pb), 100.0 * sat1 as f64 / n1 as f64, 100.0 * sat2 as f64 / n2 as f64);
        }
        let held_line = if held.is_empty() { String::new() } else { format!("\n      held out: {}", check(&net, &held).line()) };
        println!("  epoch {} ({:.0} s): training: {}{}", epoch + 1, started.elapsed().as_secs_f64(), seen.line(), held_line);
    }
    let note = serde_json::json!({ "positions": total, "files": files, "epochs": epochs, "lambda": lambda, "init": init });
    net.save(std::path::Path::new(&out), &note).expect("cannot write the network");
    println!("aztrain: wrote {out}");
}
