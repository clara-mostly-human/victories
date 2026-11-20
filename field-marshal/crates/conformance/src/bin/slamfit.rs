//! slamfit: what a slam threat and a loaded slam are worth to the leaf, in hp.
//!
//!   slamfit <replay dirs> [--bots A,B,...] [--gen 13]
//!
//! Every third tick of every game (both bots in `--bots`, if given): the slam threat each fighter
//! holds (`valuefeat::slam_threat`), whether each has dash and repulsor both ready ("loaded"), the
//! linear judgment row `--gen`, and the discounted hp fighter 0 went on to gain or lose (0.98 a
//! tick, 40 for a kill). Least squares of (hp to come - judgment) on the threat difference and the
//! loaded difference gives their worth beyond what the judgment already counts. Also how often
//! each happens and what follows a threat (hp to come when only fighter 0 holds one).

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, State};
use victris_engine::Engine;

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

fn loaded(e: &Engine, s: &State, x: usize) -> bool {
    let f = &s.fighters[x];
    let burst = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Burst { .. }));
    f.state == FState::Idle && f.cd_dash == 0 && burst.map(|b| f.cd_attack[b] == 0).unwrap_or(false)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let gen: usize = opt("--gen").and_then(|s| s.parse().ok()).unwrap_or(13);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" || a == "--gen" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let w = victris_engine::models::value_weights(gen);
    // rows: (threat diff, loaded diff, residual)
    let mut rows: Vec<(f64, f64, f64)> = Vec::new();
    let (mut threat_ticks, mut loaded_ticks, mut ticks) = (0usize, 0usize, 0usize);
    let (mut only0, mut only0_sum) = (0usize, 0.0f64);
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        if !bots.is_empty() && !replay.bots.iter().all(|b| bots.contains(b)) {
            continue;
        }
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut rec: Vec<(usize, f64, f64, f64)> = Vec::new();
        let mut reward: Vec<f64> = Vec::new();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            if ti % 3 == 0 && s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive {
                let th = victris_engine::valuefeat::slam_threat(e, &s, 0) as i32 - victris_engine::valuefeat::slam_threat(e, &s, 1) as i32;
                let ld = loaded(e, &s, 0) as i32 - loaded(e, &s, 1) as i32;
                let j = victris_engine::valuefeat::value(e, &s, 0, w) as f64;
                rec.push((ti, th as f64, ld as f64, j));
                ticks += 2;
                threat_ticks += victris_engine::valuefeat::slam_threat(e, &s, 0) as usize + victris_engine::valuefeat::slam_threat(e, &s, 1) as usize;
                loaded_ticks += loaded(e, &s, 0) as usize + loaded(e, &s, 1) as usize;
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
        for (ti, th, ld, j) in rec {
            rows.push((th, ld, to_come[ti] - j));
            if th > 0.5 {
                only0 += 1;
                only0_sum += to_come[ti];
            }
        }
    }
    // least squares, two features, no intercept
    let (mut a11, mut a12, mut a22, mut b1, mut b2) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (x1, x2, y) in &rows {
        a11 += x1 * x1;
        a12 += x1 * x2;
        a22 += x2 * x2;
        b1 += x1 * y;
        b2 += x2 * y;
    }
    let det = a11 * a22 - a12 * a12;
    let (w1, w2) = if det.abs() > 1e-9 { ((b1 * a22 - b2 * a12) / det, (a11 * b2 - a12 * b1) / det) } else { (0.0, 0.0) };
    println!("{} positions from {} files: a fighter holds a slam threat on {:.1}% of fighter-ticks, is loaded (dash and repulsor ready) on {:.1}%", rows.len(), files.len(), 100.0 * threat_ticks as f64 / ticks.max(1) as f64, 100.0 * loaded_ticks as f64 / ticks.max(1) as f64);
    println!("worth beyond judgment row {gen}: slam threat {w1:.1} hp, loaded {w2:.1} hp; hp to come when only fighter 0 holds a threat {:.1} ({} positions)", only0_sum / only0.max(1) as f64, only0);
}
