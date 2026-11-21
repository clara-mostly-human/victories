//! calib: is the search's move model calibrated against what strong bots actually play?
//!
//!   calib <replay dirs> --bot NAME [--side them|us]
//!
//! At every decision tick of the chosen side (default: the named bot's opponents), the move list
//! the search uses (`actions_with`, even 0.25, dash + repulsor) gives each turn a prior. Turns
//! fall into classes (movement x attack). Per distance band, prints each class's mean prior next
//! to how often the side really played it, and the ratio. A ratio far from 1 is a class the
//! search's model of that side gets wrong.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::{add, chebyshev, DIR_VEC};
use victris_engine::mcts::actions_with;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, State};
use victris_engine::turn::{AttackIn, DirIn, RawTurn};
use victris_engine::Engine;

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

fn class(e: &Engine, s: &State, x: usize, t: &RawTurn) -> String {
    let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
    let d = chebyshev(f.pos, o.pos);
    let rel = |dir: u8, n: i32| {
        let mut p = f.pos;
        for _ in 0..n {
            p = add(p, DIR_VEC[dir as usize]);
        }
        let a = chebyshev(p, o.pos);
        if a < d { "in" } else if a > d { "out" } else { "side" }
    };
    let mv = match (t.mv, t.dash) {
        (_, DirIn::Dir(dd)) => format!("dash {}", rel(dd, 2)),
        (DirIn::Dir(m), _) => format!("step {}", rel(m, 1)),
        _ => "hold".to_string(),
    };
    let atk = match t.attack {
        AttackIn::Named { attack, .. } => match e.rules.attacks[attack as usize].kind {
            AttackKind::Projectile { .. } => "+shot",
            AttackKind::Area { .. } => "+frost",
            AttackKind::Burst { .. } => "+repulsor",
            #[allow(unreachable_patterns)]
            _ => "+other",
        },
        _ => "",
    };
    format!("{mv}{atk}")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").expect("--bot NAME");
    let side_us = opt("--side").map(|s| s == "us").unwrap_or(false);
    let even: f32 = opt("--even").and_then(|s| s.parse().ok()).unwrap_or(0.25);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bot" || a == "--side" || a == "--even" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    const BANDS: [&str; 4] = ["1-2", "3-4", "5-7", "8+"];
    // band -> class -> (prior sum, played count)
    let mut tab: Vec<BTreeMap<String, (f64, f64)>> = vec![BTreeMap::new(); 4];
    let mut n = [0usize; 4];
    // for --emit: [band][class] (prior sum, played) by the engine's own class index
    let mut ctab = [[(0.0f64, 0.0f64); victris_engine::mcts::CLASSES]; 4];
    let emit = args.iter().any(|a| a == "--emit");
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
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        games += 1;
        let x = if side_us { me } else { 1 - me };
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let f = &s.fighters[x];
            if f.state == FState::Idle && f.alive && s.fighters[1 - x].alive {
                let d = chebyshev(f.pos, s.fighters[1 - x].pos);
                let b = if d <= 2 { 0 } else if d <= 4 { 1 } else if d <= 7 { 2 } else { 3 };
                let acts = actions_with(e, &s, x, even, false, false, false, true);
                let total: f32 = acts.iter().map(|a| a.prior).sum();
                for a in &acts {
                    tab[b].entry(class(e, &s, x, &a.turn)).or_default().0 += (a.prior / total.max(1e-9)) as f64;
                }
                tab[b].entry(class(e, &s, x, &t.turns[x])).or_default().1 += 1.0;
                for a in &acts {
                    ctab[b][victris_engine::mcts::turn_class(e, &s, x, &a.turn)].0 += (a.prior / total.max(1e-9)) as f64;
                }
                ctab[b][victris_engine::mcts::turn_class(e, &s, x, &t.turns[x])].1 += 1.0;
                n[b] += 1;
            }
            e.step(&mut s, &t.turns);
        }
    }
    println!("{games} games, side: {}", if side_us { bot.clone() } else { format!("{bot}'s opponents") });
    for b in 0..4 {
        let nb = n[b].max(1) as f64;
        println!("distance {} ({} decisions):", BANDS[b], n[b]);
        let mut rows: Vec<(&String, &(f64, f64))> = tab[b].iter().collect();
        rows.sort_by(|a, b| (b.1 .1).partial_cmp(&a.1 .1).unwrap());
        for (c, (p, k)) in rows {
            let (pm, km) = (100.0 * p / nb, 100.0 * k / nb);
            if pm < 0.5 && km < 0.5 {
                continue;
            }
            println!("  {:<18} model {:>5.1}%  played {:>5.1}%  ratio {:>5.2}", c, pm, km, km / pm.max(0.01));
        }
    }
    if emit {
        println!("pub const CALIB: [[f32; 28]; 4] = [");
        for b in 0..4 {
            let nb = n[b].max(1) as f64;
            let row: Vec<String> = (0..victris_engine::mcts::CLASSES)
                .map(|c| {
                    let (p, k) = ctab[b][c];
                    let r = ((k / nb + 0.002) / (p / nb + 0.002)).clamp(0.02, 20.0);
                    format!("{r:.3}")
                })
                .collect();
            println!("    [{}],", row.join(", "));
        }
        println!("];");
    }
}
