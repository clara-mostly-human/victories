//! valuenet: does a small network judge positions better than the weighted sum?
//!
//!   valuenet [--hidden 16] [--epochs 60] [--decay 1e-4] <dump file>...
//!
//! Reads the same dump files as `valuefit` (the value features of fighter 0 minus fighter
//! 1, then the discounted hp fighter 0 went on to gain or lose). The model is the weighted
//! sum plus a hidden layer made odd, so the judgment still flips sign with the point of
//! view:   v(x) = w.x + sum_h a_h * (tanh(u_h.x + b_h) - tanh(-u_h.x + b_h)).
//! Fitted with Adam. Every fifth block of 130 lines (about a game) is held out, and the
//! tool reports how much of the outcome the model explains there with the hidden layer and
//! without it (--hidden 0), on the same split.

use victris_engine::valuefeat::NV;

struct Net {
    h: usize,
    w: Vec<f64>,      // NV
    u: Vec<f64>,      // h * NV
    b: Vec<f64>,      // h
    a: Vec<f64>,      // h
}

impl Net {
    fn params(&self) -> usize {
        NV + self.h * NV + 2 * self.h
    }
    fn forward(&self, x: &[f64; NV]) -> f64 {
        let mut v: f64 = self.w.iter().zip(x.iter()).map(|(a, b)| a * b).sum();
        for k in 0..self.h {
            let z: f64 = self.u[k * NV..(k + 1) * NV].iter().zip(x.iter()).map(|(a, b)| a * b).sum();
            v += self.a[k] * ((z + self.b[k]).tanh() - (-z + self.b[k]).tanh());
        }
        v
    }
    /// Adds the gradient of 0.5 * (v - y)^2 to `g` (laid out w, u, b, a) and returns v.
    fn grad(&self, x: &[f64; NV], y: f64, g: &mut [f64]) -> f64 {
        let v = self.forward(x);
        let err = v - y;
        for i in 0..NV {
            g[i] += err * x[i];
        }
        let (ou, ob, oa) = (NV, NV + self.h * NV, NV + self.h * NV + self.h);
        for k in 0..self.h {
            let z: f64 = self.u[k * NV..(k + 1) * NV].iter().zip(x.iter()).map(|(a, b)| a * b).sum();
            let (tp, tm) = ((z + self.b[k]).tanh(), (-z + self.b[k]).tanh());
            let (dp, dm) = (1.0 - tp * tp, 1.0 - tm * tm);
            g[oa + k] += err * (tp - tm);
            g[ob + k] += err * self.a[k] * (dp - dm);
            let du = err * self.a[k] * (dp + dm);
            for i in 0..NV {
                g[ou + k * NV + i] += du * x[i];
            }
        }
        v
    }
    fn get(&mut self, i: usize) -> &mut f64 {
        let (ou, ob, oa) = (NV, NV + self.h * NV, NV + self.h * NV + self.h);
        if i < ou {
            &mut self.w[i]
        } else if i < ob {
            &mut self.u[i - ou]
        } else if i < oa {
            &mut self.b[i - ob]
        } else {
            &mut self.a[i - oa]
        }
    }
}

fn fit(train: &[([f64; NV], f64)], test: &[([f64; NV], f64)], h: usize, epochs: usize, decay: f64) -> (f64, f64) {
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rnd = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 40) as f64) / ((1u64 << 24) as f64) - 0.5
    };
    let mut net = Net { h, w: vec![0.0; NV], u: (0..h * NV).map(|_| rnd() * 0.6).collect(), b: (0..h).map(|_| rnd()).collect(), a: (0..h).map(|_| rnd() * 0.2).collect() };
    let n = net.params();
    let (mut m, mut v) = (vec![0.0; n], vec![0.0; n]);
    let mut order: Vec<usize> = (0..train.len()).collect();
    let mut step = 0usize;
    let score = |net: &Net, rows: &[([f64; NV], f64)]| {
        let (mut sse, mut sst) = (0.0, 0.0);
        for (x, y) in rows {
            sse += (net.forward(x) - y).powi(2);
            sst += y * y;
        }
        1.0 - sse / sst.max(1e-9)
    };
    let mut best = f64::MIN;
    for epoch in 0..epochs {
        for i in (1..order.len()).rev() {
            let j = ((rnd() + 0.5) * (i + 1) as f64) as usize % (i + 1);
            order.swap(i, j);
        }
        let lr = 0.01 / (1.0 + epoch as f64 / 15.0);
        for batch in order.chunks(64) {
            let mut g = vec![0.0; n];
            for &i in batch {
                net.grad(&train[i].0, train[i].1, &mut g);
            }
            step += 1;
            for k in 0..n {
                let gk = g[k] / batch.len() as f64 + decay * *net.get(k);
                m[k] = 0.9 * m[k] + 0.1 * gk;
                v[k] = 0.999 * v[k] + 0.001 * gk * gk;
                let (mh, vh) = (m[k] / (1.0 - 0.9f64.powi(step as i32)), v[k] / (1.0 - 0.999f64.powi(step as i32)));
                *net.get(k) -= lr * mh / (vh.sqrt() + 1e-8);
            }
        }
        best = best.max(score(&net, test));
    }
    (score(&net, test), best)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let hidden: usize = opt("--hidden").and_then(|s| s.parse().ok()).unwrap_or(16);
    let epochs: usize = opt("--epochs").and_then(|s| s.parse().ok()).unwrap_or(60);
    let decay: f64 = opt("--decay").and_then(|s| s.parse().ok()).unwrap_or(1e-4);
    let (mut train, mut test) = (Vec::new(), Vec::new());
    let mut skip = false;
    let mut k = 0usize;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--hidden" || a == "--epochs" || a == "--decay" {
            skip = true;
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
            // targets in units of 20 hp keep the gradients tame
            if (k / 130) % 5 == 4 { test.push((f, v[NV] / 20.0)) } else { train.push((f, v[NV] / 20.0)) }
            k += 1;
        }
    }
    println!("{} training ticks, {} held out (every fifth block of 130)", train.len(), test.len());
    let (lin, lin_best) = fit(&train, &test, 0, epochs, decay);
    println!("weighted sum alone:        explains {:.1}% of the held-out outcome (best epoch {:.1}%)", 100.0 * lin, 100.0 * lin_best);
    if hidden > 0 {
        let (net, net_best) = fit(&train, &test, hidden, epochs, decay);
        println!("with {hidden} hidden units:     explains {:.1}% of the held-out outcome (best epoch {:.1}%)", 100.0 * net, 100.0 * net_best);
    }
}
