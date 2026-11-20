//! goalvalue: what each goal of the strategy is worth in wins, from replays.
//!
//!   goalvalue <replay dirs> [--bots A,B,...]   (both fighters among the bots, if given)
//!
//! Per game and side: took the first power-up; took both; missed the first and took the other;
//! for the side that first leads by 25 hp: whether the trailer healed within the next 100 ticks.
//! Prints the win rate with and without each.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::map::PadKind;
use victris_engine::replay::parse_replay;

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    // [with: games, wins] [without: games, wins]
    let mut first = [[0usize; 2]; 2];
    let mut both = [[0usize; 2]; 2];
    let mut other = [[0usize; 2]; 2];
    let mut heal = [[0usize; 2]; 2]; // for the leader: [trailer did not heal, trailer healed]
    let mut games = 0usize;
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
        let mut takers: Vec<usize> = Vec::new();
        let mut leader: Option<(usize, i32)> = None;
        let mut trailer_healed = false;
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            if leader.is_none() {
                let d = s.fighters[0].hp - s.fighters[1].hp;
                if d >= 25 {
                    leader = Some((0, s.tick));
                } else if d <= -25 {
                    leader = Some((1, s.tick));
                }
            }
            let before = s;
            e.step(&mut s, &t.turns);
            for (i, d) in e.map.pads.iter().enumerate() {
                if before.pads[i].available && !s.pads[i].available {
                    let who = if s.fighters[0].pos == d.pos { 0 } else { 1 };
                    if d.kind == PadKind::Power {
                        takers.push(who);
                    } else if let Some((l, lt)) = leader {
                        if who != l && s.tick <= lt + 100 {
                            trailer_healed = true;
                        }
                    }
                }
            }
        }
        let Some(w) = s.result.and_then(|r| r.winner) else { continue };
        let winner = if w == 1 { 0 } else { 1 };
        games += 1;
        if let Some(&f) = takers.first() {
            for x in 0..2 {
                let k = (f == x) as usize;
                first[1 - k][0] += 1;
                first[1 - k][1] += (winner == x) as usize;
            }
            if takers.len() >= 2 {
                let x = f;
                let k = (takers[1] == f) as usize;
                both[1 - k][0] += 1;
                both[1 - k][1] += (winner == x) as usize;
                let y = 1 - f;
                let k2 = (takers[1] == y) as usize;
                other[1 - k2][0] += 1;
                other[1 - k2][1] += (winner == y) as usize;
            }
        }
        if let Some((l, _)) = leader {
            let k = trailer_healed as usize;
            heal[k][0] += 1;
            heal[k][1] += (winner == l) as usize;
        }
    }
    let r = |c: [usize; 2]| format!("{:.0}% of {}", 100.0 * c[1] as f64 / c[0].max(1) as f64, c[0]);
    println!("{games} games");
    println!("  first power-up: taker wins {}, the other side {}", r(first[0]), r(first[1]));
    println!("  with the first, also the second: wins {}; the opponent got the second: {}", r(both[0]), r(both[1]));
    println!("  missed the first, took the other: wins {}; missed both: {}", r(other[0]), r(other[1]));
    println!("  first to lead by 25: wins {} when the trailer did not heal in the next 100 ticks, {} when it healed", r(heal[0]), r(heal[1]));
}
