//! openings: how strong bots open, map by map and seat by seat.
//!
//!   openings <replay dir> --bots A,B,... [--tick 19] [--show 6]
//!
//! For every game where the named bot plays, records where it stands at `--tick` (the power
//! pads first appear at tick 20) and the line of turns it played to get there. Prints, per
//! map and seat, the most common tiles at that tick and the most common opening lines, so
//! the opening can be solved against what they actually do.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use victris_engine::geom::DIR_NAMES;
use victris_engine::replay::parse_replay;
use victris_engine::turn::{AttackIn, DirIn, RawTurn};

fn short(t: &RawTurn) -> String {
    let mut s = String::new();
    if let DirIn::Dir(d) = t.mv {
        s.push_str(DIR_NAMES[d as usize]);
    }
    if let DirIn::Dir(d) = t.dash {
        s.push_str(&format!("D{}", DIR_NAMES[d as usize]));
    }
    if let AttackIn::Named { .. } = t.attack {
        s.push('*');
    }
    if s.is_empty() {
        "-".into()
    } else {
        s
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").expect("--bots").split(',').map(str::to_string).collect();
    let at: i32 = opt("--tick").and_then(|s| s.parse().ok()).unwrap_or(19);
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(6);
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    // (map, seat) -> tile counts, line counts
    let mut tiles: HashMap<(String, usize), HashMap<[i32; 2], usize>> = HashMap::new();
    let mut lines: HashMap<(String, usize), HashMap<String, usize>> = HashMap::new();
    let mut games: HashMap<(String, usize), usize> = HashMap::new();
    for path in files {
        let name = path.to_string_lossy().to_string();
        if !name.ends_with(".jsonl.gz") {
            continue;
        }
        let mut text = String::new();
        if std::fs::File::open(&path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        for x in 0..2 {
            if !replay.bots.get(x).map(|b| bots.contains(b)).unwrap_or(false) {
                continue;
            }
            let mut s = e.new_match(replay.seed);
            let mut line = Vec::new();
            for t in &replay.ticks {
                if s.tick >= at || s.result.is_some() {
                    break;
                }
                line.push(short(&t.turns[x]));
                e.step(&mut s, &t.turns);
            }
            if s.tick < at {
                continue;
            }
            let key = (e.map.name.clone(), x);
            *games.entry(key.clone()).or_default() += 1;
            *tiles.entry(key.clone()).or_default().entry(s.fighters[x].pos).or_default() += 1;
            *lines.entry(key).or_default().entry(line.join(" ")).or_default() += 1;
        }
    }
    let mut keys: Vec<_> = games.keys().cloned().collect();
    keys.sort();
    for k in keys {
        let n = games[&k];
        println!("{} seat {}: {} games", k.0, k.1 + 1, n);
        let mut ts: Vec<_> = tiles[&k].iter().collect();
        ts.sort_by(|a, b| b.1.cmp(a.1));
        println!("  at tick {at}: {}", ts.iter().take(show).map(|(p, c)| format!("{:?} {}", p, c)).collect::<Vec<_>>().join(", "));
        let mut ls: Vec<_> = lines[&k].iter().collect();
        ls.sort_by(|a, b| b.1.cmp(a.1));
        for (l, c) in ls.iter().take(3) {
            println!("  {c:>3}x {l}");
        }
    }
}
