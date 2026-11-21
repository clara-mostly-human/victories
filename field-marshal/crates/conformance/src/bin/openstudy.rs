//! openstudy: how the opening power-up fight goes on one map, per bot.
//!
//!   openstudy <replay dirs> --map columns-21 [--bots A,B,...] [--min-games 8]
//!
//! For every game on the map, each side up to the first power pickup (pads first spawn at
//! `first_spawn`) and on to the end of that buff: where it stood the ticks before the spawn
//! (distance to the nearest power pad and to the opponent), whether it was already on a pad tile
//! the tick before the spawn ("rushed") or waited off it, what it fired and how often it dashed
//! before the spawn, who took the first power-up and when, and the hp difference at the pickup
//! and when the buff ran out. Prints one line per bot with enough games (only `--bots` if given),
//! and the outcome split by rushed / waited.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::chebyshev;
use victris_engine::map::PadKind;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::turn::{AttackIn, DirIn};

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

#[derive(Default, Clone)]
struct Acc {
    games: usize,
    wins: usize,
    took_first: usize,
    took_both: usize,
    rushed: usize,
    d_pad: [f64; 3],  // at spawn-5, spawn-2, spawn-1
    d_opp: [f64; 3],
    shots: f64,
    frosts: f64,
    repulsors: f64,
    dashes: f64,
    hp_at_pick: f64,  // own minus opponent's, at the first pickup
    hp_at_end: f64,   // at the end of that buff
    slammed: f64,
    // outcome by rushed (0) / waited (1): games, took first, hp at end sum, wins
    by: [[f64; 4]; 2],
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let map = opt("--map").unwrap_or_else(|| "columns-21".into());
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let min_games: usize = opt("--min-games").and_then(|s| s.parse().ok()).unwrap_or(8);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if ["--map", "--bots", "--min-games"].contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
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
        let Some(pk) = e.rules.pickups.as_ref() else { continue };
        let spawn = pk.first_spawn;
        let duration = pk.power.as_ref().map(|p| p.duration).unwrap_or(50);
        let pads: Vec<(usize, victris_engine::geom::P)> = e.map.pads.iter().enumerate().filter(|(_, p)| p.kind == PadKind::Power).map(|(i, p)| (i, p.pos)).collect();
        if pads.is_empty() {
            continue;
        }
        games += 1;
        let mut s = e.new_match(replay.seed);
        let mut fired = [[0usize; 3]; 2];
        let mut dashes = [0usize; 2];
        let mut d_pad = [[0.0f64; 3]; 2];
        let mut d_opp = [0.0f64; 3];
        let mut rushed = [false; 2];
        let mut takers: Vec<(usize, i32)> = Vec::new();
        let mut hp_pick = 0.0;
        let mut hp_end: Option<f64> = None;
        let mut slammed = [0usize; 2];
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let tick = s.tick;
            for (k, back) in [5, 2, 1].iter().enumerate() {
                if tick == spawn - back {
                    for x in 0..2 {
                        d_pad[x][k] = pads.iter().map(|(_, p)| chebyshev(*p, s.fighters[x].pos)).min().unwrap_or(0) as f64;
                    }
                    d_opp[k] = chebyshev(s.fighters[0].pos, s.fighters[1].pos) as f64;
                    if *back == 1 {
                        for x in 0..2 {
                            rushed[x] = pads.iter().any(|(_, p)| *p == s.fighters[x].pos);
                        }
                    }
                }
            }
            if tick < spawn {
                for x in 0..2 {
                    let tu = &t.turns[x];
                    if let AttackIn::Named { attack, .. } = tu.attack {
                        match e.rules.attacks[attack as usize].kind {
                            AttackKind::Projectile { .. } => fired[x][0] += 1,
                            AttackKind::Area { .. } => fired[x][1] += 1,
                            AttackKind::Burst { .. } => fired[x][2] += 1,
                            #[allow(unreachable_patterns)]
                            _ => {}
                        }
                    }
                    if matches!(tu.dash, DirIn::Dir(_)) {
                        dashes[x] += 1;
                    }
                }
            }
            let before = s;
            let sum = e.step(&mut s, &t.turns);
            if takers.len() < 2 {
                for (i, p) in &pads {
                    if before.pads[*i].available && !s.pads[*i].available {
                        let who = if s.fighters[0].pos == *p { 0 } else { 1 };
                        if takers.is_empty() {
                            hp_pick = (s.fighters[0].hp - s.fighters[1].hp) as f64;
                        }
                        takers.push((who, s.tick));
                    }
                }
            }
            if let Some(&(_, t0)) = takers.first() {
                if hp_end.is_none() && s.tick >= t0 + duration {
                    hp_end = Some((s.fighters[0].hp.max(0) - s.fighters[1].hp.max(0)) as f64);
                }
                if s.tick <= t0 + duration {
                    for x in 0..2 {
                        slammed[x] += sum.slammed[x] as usize;
                    }
                }
            }
        }
        let end = hp_end.unwrap_or((s.fighters[0].hp.max(0) - s.fighters[1].hp.max(0)) as f64);
        let winner = s.result.and_then(|r| r.winner).map(|w| w as usize - 1);
        for x in 0..2 {
            let sign = if x == 0 { 1.0 } else { -1.0 };
            let a = acc.entry(replay.bots[x].clone()).or_default();
            a.games += 1;
            a.wins += (winner == Some(x)) as usize;
            let first = takers.first().map(|t| t.0 == x).unwrap_or(false);
            a.took_first += first as usize;
            a.took_both += (takers.len() >= 2 && takers[0].0 == x && takers[1].0 == x) as usize;
            a.rushed += rushed[x] as usize;
            for k in 0..3 {
                a.d_pad[k] += d_pad[x][k];
                a.d_opp[k] += d_opp[k];
            }
            a.shots += fired[x][0] as f64;
            a.frosts += fired[x][1] as f64;
            a.repulsors += fired[x][2] as f64;
            a.dashes += dashes[x] as f64;
            a.hp_at_pick += sign * hp_pick;
            a.hp_at_end += sign * end;
            a.slammed += slammed[x] as f64;
            let r = if rushed[x] { 0 } else { 1 };
            a.by[r][0] += 1.0;
            a.by[r][1] += first as usize as f64;
            a.by[r][2] += sign * end;
            a.by[r][3] += (winner == Some(x)) as usize as f64;
        }
    }
    println!("{games} games on {map}");
    let mut rows: Vec<(&String, &Acc)> = acc.iter().filter(|(n, a)| a.games >= min_games && (bots.is_empty() || bots.contains(n))).collect();
    rows.sort_by(|a, b| (b.1.hp_at_end / b.1.games as f64).partial_cmp(&(a.1.hp_at_end / a.1.games as f64)).unwrap());
    for (name, a) in rows {
        let g = a.games as f64;
        let by = |r: usize| {
            let b = a.by[r];
            if b[0] < 1.0 {
                "-".to_string()
            } else {
                format!("{:.0} games, first pad {:.0}%, hp at buff end {:+.1}, won {:.0}%", b[0], 100.0 * b[1] / b[0], b[2] / b[0], 100.0 * b[3] / b[0])
            }
        };
        println!(
            "{name}: {} games, won {:.0}% | took the first power-up {:.0}%, both {:.0}% | hp diff at the pickup {:+.1}, at buff end {:+.1} | slammed {:.2} a game by then | before the spawn: pad distance {:.1}/{:.1}/{:.1} and opponent distance {:.1}/{:.1}/{:.1} at -5/-2/-1, on a pad at -1 {:.0}%, shots {:.1}, frosts {:.1}, repulsors {:.1}, dashes {:.1}",
            a.games, 100.0 * a.wins as f64 / g, 100.0 * a.took_first as f64 / g, 100.0 * a.took_both as f64 / g,
            a.hp_at_pick / g, a.hp_at_end / g, a.slammed / g,
            a.d_pad[0] / g, a.d_pad[1] / g, a.d_pad[2] / g, a.d_opp[0] / g, a.d_opp[1] / g, a.d_opp[2] / g,
            100.0 * a.rushed as f64 / g, a.shots / g, a.frosts / g, a.repulsors / g, a.dashes / g
        );
        println!("    rushed: {} | waited: {}", by(0), by(1));
    }
}
