//! aztrain: teaches the network (`pvnet`) from self-play files written by `azgen`.
//!
//! The value learns who went on to win (mixed with the search's own value where it has one);
//! the turn shares learn where the search put its visits.
//!
//!   aztrain --out NET.json --data FILE [FILE ...] [--init NET.json] [--h1 64] [--h2 32]
//!           [--epochs 8] [--lr 0.002] [--batch 512] [--lambda 0.5] [--pw 1.0] [--threads 4]
//!           [--holdout 0.05] [--seed 1] [--map columns-21] [--mirror 1]
//!
//! With --mirror (on by default) every position is shown in a mirror image of the map drawn
//! afresh each time (`pvnet::Sym`), where the map mirrors onto itself.

use victris_engine::pvnet::{layout, loss_grad, Loss, PvNet, Rec, Scratch, Sym, REC};
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
}

impl Sum {
    fn add(&mut self, l: &Loss) {
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
    }
    fn line(&self) -> String {
        format!(
            "value loss {:.4}, picks the winner {:.1}%; moves loss {:.4} (the search's own spread {:.4}), same first choice as the search {:.1}%",
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
    let (h1, h2): (usize, usize) = (arg(&args, "--h1", 64), arg(&args, "--h2", 32));
    let epochs: usize = arg(&args, "--epochs", 8);
    let lr0: f32 = arg(&args, "--lr", 0.002);
    let batch: usize = arg(&args, "--batch", 512);
    let lambda: f32 = arg(&args, "--lambda", 0.5);
    let pw: f32 = arg(&args, "--pw", 1.0);
    let threads: usize = arg(&args, "--threads", 4);
    let holdout: f64 = arg(&args, "--holdout", 0.05);
    let seed: u64 = arg(&args, "--seed", 1);
    let map: String = arg(&args, "--map", String::from("columns-21"));
    let mirror: u32 = arg(&args, "--mirror", 1);
    let sym = Sym::of(&victris_engine::Engine::shipped(&map));
    let images: Vec<usize> = (0..4).filter(|k| *k == 0 || (mirror != 0 && sym.ok[*k])).collect();
    let files: Vec<String> = match args.iter().position(|a| a == "--data") {
        Some(i) => args[i + 1..].iter().take_while(|a| !a.starts_with("--")).cloned().collect(),
        None => Vec::new(),
    };
    let mut data: Vec<u8> = Vec::new();
    for f in &files {
        let b = std::fs::read(f).unwrap_or_else(|_| panic!("cannot read {f}"));
        data.extend_from_slice(&b[..b.len() / REC * REC]);
    }
    let total = data.len() / REC;
    assert!(total > 0, "no positions to learn from");
    let mut rng = Rng::new(seed);
    // positions of one game sit together in the file: hold out whole runs of 256, so the check
    // is on games the network did not train on
    let mut train: Vec<u32> = Vec::new();
    let mut held: Vec<u32> = Vec::new();
    let mut block_held = false;
    for i in 0..total {
        if i % 256 == 0 {
            block_held = rng.unit() < holdout;
        }
        if block_held {
            held.push(i as u32);
        } else {
            train.push(i as u32);
        }
    }
    let mut net = if init.is_empty() { PvNet::blank(&map, h1, h2, seed) } else { PvNet::load(std::path::Path::new(&init)).expect("cannot read --init") };
    let len = layout(net.h1, net.h2).len;
    println!("aztrain: {} positions ({} held out) from {} files, network {}x{} ({} weights), {} epochs, {} mirror images", total, held.len(), files.len(), net.h1, net.h2, len, epochs, images.len());
    let rec = |i: u32| Rec::read(&data[i as usize * REC..(i as usize + 1) * REC]);
    let check = |net: &PvNet, ids: &[u32]| -> Sum {
        let chunk = ids.len().div_ceil(threads.max(1)).max(1);
        std::thread::scope(|sc| {
            let hs: Vec<_> = ids.chunks(chunk).map(|part| sc.spawn(move || {
                let (mut sum, mut scr) = (Sum::default(), Scratch::default());
                for &i in part {
                    sum.add(&loss_grad(net, &rec(i), lambda, pw, &mut scr, None));
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
                        let r = if k == 0 { rec(i) } else { sym_ref.image(k, &rec(i)) };
                        sum.add(&loss_grad(net_ref, &r, lambda, pw, &mut scr, Some(&mut grad)));
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
                net.w[k] -= lr * (m[k] / c1) / ((v[k] / c2).sqrt() + 1e-8);
            }
            for p in &parts {
                seen.merge(&p.1);
            }
        }
        let held_line = if held.is_empty() { String::new() } else { format!("\n      held out: {}", check(&net, &held).line()) };
        println!("  epoch {} ({:.0} s): training: {}{}", epoch + 1, started.elapsed().as_secs_f64(), seen.line(), held_line);
    }
    let note = serde_json::json!({ "positions": total, "files": files, "epochs": epochs, "lambda": lambda, "init": init });
    net.save(std::path::Path::new(&out), &note).expect("cannot write the network");
    println!("aztrain: wrote {out}");
}
