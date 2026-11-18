//! Conformance and benchmark tool for the engine port.
//!
//!   conformance verify <file-or-dir>...   re-simulate replays (.jsonl or .jsonl.gz) and
//!                                         check every tick hash and the result
//!   conformance bench [seconds]           measure ticks per second on one core
//!
//! `verify` also accepts games written by `tools/fuzz-oracle.mjs`, which plays random
//! turns through the official engine and records the full state after every tick.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use victris_engine::replay::{parse_replay, verify_replay};
use victris_engine::turn::RawTurn;
use victris_engine::{Engine, State};

fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = String::new();
        GzDecoder::new(&bytes[..]).read_to_string(&mut out).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(out)
    } else {
        String::from_utf8(bytes).map_err(|e| format!("{}: {e}", path.display()))
    }
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".jsonl") || name.ends_with(".jsonl.gz") {
            out.push(path.to_path_buf());
        }
    }
}

/// First position where two strings differ, with some context, for a readable report.
fn show_diff(ours: &str, theirs: &str) {
    let i = ours.bytes().zip(theirs.bytes()).position(|(a, b)| a != b).unwrap_or(ours.len().min(theirs.len()));
    let lo = i.saturating_sub(160);
    let cut = |s: &str| s.get(lo..(i + 120).min(s.len())).unwrap_or("").to_string();
    eprintln!("    first difference at byte {i}");
    eprintln!("    ours:   …{}", cut(ours));
    eprintln!("    theirs: …{}", cut(theirs));
}

/// A multi-game file from the fuzz oracle holds several replays back to back; each starts
/// with a header line.
fn split_games(text: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = Vec::new();
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("{\"type\":\"header\"") {
            starts.push(pos);
        }
        pos += line.len();
    }
    if starts.is_empty() {
        return vec![text];
    }
    starts.iter().enumerate().map(|(i, &s)| &text[s..starts.get(i + 1).copied().unwrap_or(text.len())]).collect()
}

#[derive(Default)]
struct Tally {
    games: usize,
    ok: usize,
    failed: usize,
    ticks: usize,
    hashes: usize,
    observations: usize,
    results: std::collections::BTreeMap<String, usize>,
}

fn verify_game(label: &str, game: &str, t: &mut Tally) {
    t.games += 1;
    let replay = match parse_replay(game) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("FAIL {label}: {e}");
            t.failed += 1;
            return;
        }
    };
    match verify_replay(&replay) {
        Ok(v) => {
            t.ok += 1;
            t.ticks += v.ticks;
            t.hashes += v.hashes_checked;
            t.observations += v.observations_checked;
            let reason = replay.result.as_ref().map(|r| r.reason.clone()).unwrap_or_else(|| "(no result line)".into());
            *t.results.entry(reason).or_default() += 1;
        }
        Err(d) => {
            t.failed += 1;
            eprintln!("FAIL {label}: tick {}: {} (map {}, seed {})", d.tick, d.message, replay.engine.map.name, replay.seed);
            if let (Some(ours), Some(theirs)) = (&d.ours, &d.theirs) {
                show_diff(ours, theirs);
            } else if let Some(ours) = &d.ours {
                eprintln!("    ours: {ours}");
            }
        }
    }
}

fn verify(paths: &[String]) -> i32 {
    let t0 = Instant::now();
    let mut t = Tally::default();
    if paths.len() == 1 && paths[0] == "-" {
        // Stream games from stdin (the fuzz oracle's output), one game in memory at a time.
        let stdin = std::io::stdin();
        let mut game = String::new();
        let mut line = String::new();
        let mut n = 0usize;
        loop {
            line.clear();
            let read = std::io::BufRead::read_line(&mut stdin.lock(), &mut line).unwrap_or(0);
            if read == 0 || line.starts_with("{\"type\":\"header\"") {
                if !game.is_empty() {
                    verify_game(&format!("stdin#{n}"), &game, &mut t);
                    n += 1;
                    game.clear();
                }
                if read == 0 {
                    break;
                }
            }
            game.push_str(&line);
        }
    } else {
        let mut files = Vec::new();
        for p in paths {
            collect(Path::new(p), &mut files);
        }
        if files.is_empty() {
            eprintln!("no replay files found");
            return 2;
        }
        for f in &files {
            match read_text(f) {
                Ok(text) => {
                    for (gi, game) in split_games(&text).into_iter().enumerate() {
                        verify_game(&format!("{}#{gi}", f.display()), game, &mut t);
                    }
                }
                Err(e) => {
                    eprintln!("FAIL {e}");
                    t.failed += 1;
                }
            }
        }
    }
    println!(
        "{} of {} games verified, {} failed; {} ticks re-simulated, {} state hashes matched; {:.2}s",
        t.ok,
        t.games,
        t.failed,
        t.ticks,
        t.hashes,
        t0.elapsed().as_secs_f64()
    );
    if t.observations > 0 {
        println!("{} official observations matched", t.observations);
    }
    println!("results: {:?}", t.results);
    if t.failed == 0 && t.games > 0 {
        0
    } else {
        1
    }
}

// The generator analysis/bench.mjs uses, reproduced bit for bit (including JavaScript's
// double-precision multiply), so both benchmarks play exactly the same games.
struct Lcg(u32);
impl Lcg {
    fn next(&mut self, n: u32) -> u32 {
        let x = self.0 as f64 * 1103515245.0 + 12345.0;
        self.0 = (x as i64 as u32) & 0x7fff_ffff;
        self.0 % n
    }
}

/// The same turn mix as analysis/bench.mjs, so the numbers compare with the official
/// engine's: a step 80% of the time, a dash 10%, a ranged shot at the enemy 25%, a
/// repulsor 25%.
fn rand_turn(rng: &mut Lcg, s: &State, me: usize, ranged: u8, repulsor: u8) -> RawTurn {
    let mut t = RawTurn::IDLE;
    let m = rng.next(10);
    if m < 8 {
        t = RawTurn::step(m as u8);
    } else if m == 8 {
        t = RawTurn::dash(rng.next(8) as u8);
    }
    match rng.next(4) {
        0 => t.with_attack(ranged, s.fighters[1 - me].pos),
        1 => t.with_burst(repulsor),
        _ => t,
    }
}

fn bench(seconds: f64) {
    let engines: Vec<Engine> = victris_engine::shipped::MAP_NAMES.iter().map(|n| Engine::shipped(n)).collect();
    let ranged = engines[0].rules.attack_index("ranged").unwrap();
    let repulsor = engines[0].rules.attack_index("repulsor").unwrap();
    println!("state size: {} bytes", std::mem::size_of::<State>());
    for mode in ["step", "step+clone", "step+hash"] {
        let mut rng = Lcg(12345);
        let (mut ticks, mut games) = (0u64, 0u64);
        let mut sink = 0u64;
        let t0 = Instant::now();
        while t0.elapsed().as_secs_f64() < seconds {
            let e = &engines[(games % 3) as usize];
            let mut s = e.new_match(games as i64);
            games += 1;
            while s.result.is_none() {
                if mode == "step+clone" {
                    // A search copies the state before every simulated tick.
                    let copy = std::hint::black_box(s);
                    s = copy;
                }
                let turns = [rand_turn(&mut rng, &s, 0, ranged, repulsor), rand_turn(&mut rng, &s, 1, ranged, repulsor)];
                e.step(&mut s, &turns);
                if mode == "step+hash" {
                    sink ^= e.state_hash(&s).len() as u64;
                }
                ticks += 1;
            }
            sink ^= s.fighters[0].hp as u64;
        }
        let dt = t0.elapsed().as_secs_f64();
        std::hint::black_box(sink);
        println!(
            "{mode:<12} {:>12.0} ticks/s  {:>8.0} games/s  avg len {:.0}",
            ticks as f64 / dt,
            games as f64 / dt,
            ticks as f64 / games as f64
        );
    }
}

/// Fits the tempo evaluation from replays: for every tick of every game, from fighter 1's
/// side, regress the hp swing over the next `horizon` ticks on the tempo features.
/// Ridge regression by normal equations; prints the weights as a Rust array.
fn fit_eval(paths: &[String], horizon: usize) {
    use victris_engine::tempo::{diff, N, NAMES};
    let mut files = Vec::new();
    for p in paths {
        collect(Path::new(p), &mut files);
    }
    let mut xtx = vec![0.0f64; N * N];
    let mut xty = vec![0.0f64; N];
    let (mut n, mut sum_y, mut sum_yy) = (0u64, 0.0f64, 0.0f64);
    let mut rows: Vec<([f32; N], f32)> = Vec::new();
    for f in &files {
        let Ok(text) = read_text(f) else { continue };
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut feats: Vec<[f32; N]> = Vec::new();
        let mut hp: Vec<[i32; 2]> = Vec::new();
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            feats.push(diff(e, &s, 0));
            hp.push([s.fighters[0].hp, s.fighters[1].hp]);
            e.step(&mut s, &t.turns);
        }
        hp.push([s.fighters[0].hp, s.fighters[1].hp]);
        for (i, x) in feats.iter().enumerate() {
            let j = (i + horizon).min(hp.len() - 1);
            // hp the opponent lost minus hp we lost (pickups count as negative losses)
            let y = ((hp[i][1] - hp[j][1]) - (hp[i][0] - hp[j][0])) as f32;
            rows.push((*x, y));
        }
    }
    for (x, y) in &rows {
        for a in 0..N {
            xty[a] += x[a] as f64 * *y as f64;
            for b in 0..N {
                xtx[a * N + b] += x[a] as f64 * x[b] as f64;
            }
        }
        n += 1;
        sum_y += *y as f64;
        sum_yy += (*y as f64).powi(2);
    }
    // Ridge, then Gaussian elimination with partial pivoting.
    let lambda = 1e-3 * n as f64;
    let mut a = xtx.clone();
    let mut b = xty.clone();
    for i in 0..N {
        a[i * N + i] += lambda;
    }
    for col in 0..N {
        let piv = (col..N).max_by(|x, y| a[x * N + col].abs().partial_cmp(&a[y * N + col].abs()).unwrap()).unwrap();
        for k in 0..N {
            a.swap(col * N + k, piv * N + k);
        }
        b.swap(col, piv);
        let d = a[col * N + col];
        if d.abs() < 1e-12 {
            continue;
        }
        for r in 0..N {
            if r != col {
                let f = a[r * N + col] / d;
                for k in 0..N {
                    a[r * N + k] -= f * a[col * N + k];
                }
                b[r] -= f * b[col];
            }
        }
    }
    let w: Vec<f64> = (0..N).map(|i| if a[i * N + i].abs() < 1e-12 { 0.0 } else { b[i] / a[i * N + i] }).collect();
    let mut sse = 0.0;
    for (x, y) in &rows {
        let p: f64 = (0..N).map(|i| w[i] * x[i] as f64).sum();
        sse += (*y as f64 - p).powi(2);
    }
    let var = sum_yy - sum_y * sum_y / n as f64;
    println!("{} games, {n} ticks, horizon {horizon}; mean swing {:.2}, sd {:.2}; R^2 {:.4}", files.len(), sum_y / n as f64, (var / n as f64).sqrt(), 1.0 - sse / var);
    for i in 0..N {
        let share = xtx[i * N + i] / n as f64;
        println!("  {:>8.2}  {}   (nonzero on {:.1}% of ticks)", w[i], NAMES[i], 100.0 * share);
    }
    println!("pub const WEIGHTS: [f32; N] = [{}];", w.iter().map(|x| format!("{x:.3}")).collect::<Vec<_>>().join(", "));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fit-eval") if args.len() > 1 => {
            let horizon = args.iter().position(|a| a == "--horizon").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(12);
            let paths: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--") && a.parse::<usize>().is_err()).cloned().collect();
            fit_eval(&paths, horizon)
        }
        Some("verify") if args.len() > 1 => std::process::exit(verify(&args[1..])),
        Some("bench") => bench(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(3.0)),
        _ => {
            eprintln!("usage: conformance verify <file-or-dir>... | conformance bench [seconds]");
            std::process::exit(2);
        }
    }
}
