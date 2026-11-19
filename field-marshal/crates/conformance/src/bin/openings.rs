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

/// The move part of a turn as book JSON: {"move": dir} or {"dash": dir} or {} (hold).
fn move_json(t: &RawTurn) -> serde_json::Value {
    let mut o = serde_json::json!({});
    if let DirIn::Dir(d) = t.mv {
        o["move"] = serde_json::json!(DIR_NAMES[d as usize]);
    }
    if let DirIn::Dir(d) = t.dash {
        o["dash"] = serde_json::json!(DIR_NAMES[d as usize]);
    }
    o
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").expect("--bots").split(',').map(str::to_string).collect();
    // --vs A,B,...: only games where the other seat is one of these bots (books per opponent type)
    let vs: Vec<String> = opt("--vs").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let at: i32 = opt("--tick").and_then(|s| s.parse().ok()).unwrap_or(19);
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(6);
    let dir = args.iter().enumerate().find(|(i, a)| !a.starts_with("--") && (*i == 0 || (args[i - 1] != "--book-out" && args[i - 1] != "--vs")) && std::path::Path::new(a).is_dir()).map(|(_, a)| a).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    // (map, seat) -> tile counts, line counts
    let mut tiles: HashMap<(String, usize), HashMap<[i32; 2], usize>> = HashMap::new();
    let mut lines: HashMap<(String, usize), HashMap<String, usize>> = HashMap::new();
    let mut games: HashMap<(String, usize), usize> = HashMap::new();
    // --book-out dir: per map and seat, for each (tick, tile) the strong bots stood on before
    // `--tick`, the move they made most often from there (with `--min` games or more and at
    // least 40% of them): a book of what strong bots do in the opening, as `<map>-<seat>.json`
    let book_out = opt("--book-out");
    let min: usize = opt("--min").and_then(|s| s.parse().ok()).unwrap_or(5);
    let mut moves: HashMap<(String, usize), HashMap<(i32, [i32; 2]), HashMap<String, (usize, serde_json::Value)>>> = HashMap::new();
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
            if !vs.is_empty() && !replay.bots.get(1 - x).map(|b| vs.contains(b)).unwrap_or(false) {
                continue;
            }
            let mut s = e.new_match(replay.seed);
            let mut line = Vec::new();
            for t in &replay.ticks {
                if s.tick >= at || s.result.is_some() {
                    break;
                }
                let mj = move_json(&t.turns[x]);
                let cell = moves.entry((e.map.name.clone(), x)).or_default().entry((s.tick, s.fighters[x].pos)).or_default().entry(mj.to_string()).or_insert((0, mj));
                cell.0 += 1;
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
    if let Some(dir) = &book_out {
        for k in &keys {
            let mut book = Vec::new();
            if let Some(m) = moves.get(k) {
                let mut cells: Vec<_> = m.iter().collect();
                cells.sort_by_key(|((t, p), _)| (*t, p[0], p[1]));
                for ((tick, pos), counts) in cells {
                    let total: usize = counts.values().map(|c| c.0).sum();
                    let Some((_, (n, turn))) = counts.iter().max_by_key(|(_, c)| c.0) else { continue };
                    if *n >= min && *n * 10 >= total * 4 {
                        book.push(serde_json::json!({ "tick": tick, "pos": pos, "turn": turn, "games": n, "of": total }));
                    }
                }
            }
            let doc = serde_json::json!({ "map": k.0, "seat": k.1 + 1, "samples": games[k], "book": book });
            let path = format!("{dir}/{}-{}.json", k.0, k.1 + 1);
            std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).expect("write the book");
            println!("{path}: {} entries", doc["book"].as_array().map(|a| a.len()).unwrap_or(0));
        }
    }
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
