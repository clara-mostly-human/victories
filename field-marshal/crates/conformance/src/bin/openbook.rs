//! openbook: solve the opening against what the strong bots actually play.
//!
//!   openbook <replay dir> --bots A,B,... --map NAME --seat 1|2 [--samples 50] [--ticks 30]
//!            [--iterations 40000] [--gen 6] [--dash-burst] [--out file.json]
//!
//! Takes up to `--samples` real opening lines (turns from tick 0) played by the named bots in
//! the other seat on this map. Then builds our line one tick at a time: at each tick every
//! sample's position (our line so far against that sample's line so far) is searched with the
//! opponent's turn at the root fixed to what that bot really played, and we take the turn
//! with the best average result over all samples. Writes, per tick, our turn and the tile we
//! stand on before it, as JSON for the playbook's opening book.

use flate2::read::GzDecoder;
use serde_json::{json, Value};
use std::io::Read;
use victris_engine::geom::DIR_NAMES;
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::state::State;
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
use victris_engine::Engine;

fn to_json(e: &Engine, t: &RawTurn) -> Value {
    let mut o = json!({});
    if let DirIn::Dir(d) = t.mv {
        o["move"] = json!(DIR_NAMES[d as usize]);
    }
    if let DirIn::Dir(d) = t.dash {
        o["dash"] = json!(DIR_NAMES[d as usize]);
    }
    if let AttackIn::Named { attack, target } = t.attack {
        let name = &e.rules.attacks[attack as usize].name;
        o["attack"] = match target {
            TargetIn::Tile(p) => json!({ "name": name, "target": p }),
            _ => json!({ "name": name }),
        };
    }
    o
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").expect("--bots").split(',').map(str::to_string).collect();
    let map = opt("--map").expect("--map");
    let seat: usize = opt("--seat").and_then(|s| s.parse().ok()).expect("--seat 1|2");
    let me = seat - 1;
    let samples: usize = opt("--samples").and_then(|s| s.parse().ok()).unwrap_or(50);
    let ticks: usize = opt("--ticks").and_then(|s| s.parse().ok()).unwrap_or(30);
    let iterations: u32 = opt("--iterations").and_then(|s| s.parse().ok()).unwrap_or(40_000);
    let gen: usize = opt("--gen").and_then(|s| s.parse().ok()).unwrap_or(6);
    let dash_burst = args.iter().any(|a| a == "--dash-burst");
    let out = opt("--out");
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    // Their lines: (engine, seed, turns of the other seat for the first `ticks` ticks).
    let mut lines: Vec<(Engine, i64, Vec<RawTurn>)> = Vec::new();
    for path in files.iter().rev() {
        if lines.len() >= samples {
            break;
        }
        let name = path.to_string_lossy().to_string();
        if !name.ends_with(".jsonl.gz") {
            continue;
        }
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        if replay.engine.map.name != map || !replay.bots.get(1 - me).map(|b| bots.contains(b)).unwrap_or(false) {
            continue;
        }
        if replay.ticks.len() < ticks {
            continue;
        }
        let theirs: Vec<RawTurn> = replay.ticks.iter().take(ticks).map(|t| t.turns[1 - me]).collect();
        lines.push((replay.engine.clone(), replay.seed, theirs));
    }
    eprintln!("{} opening lines of the other seat on {map}", lines.len());
    if lines.is_empty() {
        return;
    }
    let e0 = lines[0].0.clone();
    let mut states: Vec<State> = lines.iter().map(|(e, seed, _)| e.new_match(*seed)).collect();
    let cfg = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst, max_nodes: 120_000, ..Default::default() };
    let mut book = Vec::new();
    for t in 0..ticks {
        // Average result of each of our turns over the samples still in play.
        let mut score: Vec<(RawTurn, (f64, usize))> = Vec::new();
        let mut live = 0usize;
        let pos_before = states[0].fighters[me].pos;
        for (k, (e, _, theirs)) in lines.iter().enumerate() {
            let s = &states[k];
            if s.result.is_some() {
                continue;
            }
            live += 1;
            let mut fixed = [None, None];
            fixed[1 - me] = Some(theirs[t]);
            let mut search = Search::new(e, cfg);
            search.start();
            search.iterate(s, fixed, iterations);
            let root = search.result();
            for (turn, n, q) in root.acts[me].iter() {
                if *n == 0 {
                    continue;
                }
                match score.iter_mut().find(|(t, _)| t == turn) {
                    Some((_, v)) => {
                        v.0 += q;
                        v.1 += 1;
                    }
                    None => score.push((*turn, (*q, 1))),
                }
            }
        }
        // Turns seen in at least 80% of the samples, by average result.
        let need = (live as f64 * 0.8).ceil() as usize;
        let best = score.iter().filter(|(_, v)| v.1 >= need.max(1)).max_by(|a, b| (a.1 .0 / a.1 .1 as f64).partial_cmp(&(b.1 .0 / b.1 .1 as f64)).unwrap()).map(|(t, v)| (*t, v.0 / v.1 as f64));
        let Some((ours, avg)) = best else { break };
        for (k, (e, _, theirs)) in lines.iter().enumerate() {
            if states[k].result.is_none() {
                let mut turns = [RawTurn::IDLE; 2];
                turns[me] = ours;
                turns[1 - me] = theirs[t];
                e.step(&mut states[k], &turns);
            }
        }
        eprintln!("tick {t:>2}: {} (average {avg:+.1} hp over {live})", to_json(&e0, &ours));
        book.push(json!({ "tick": t, "pos": pos_before, "turn": to_json(&e0, &ours) }));
    }
    let doc = json!({ "map": map, "seat": seat, "samples": lines.len(), "book": book });
    match out {
        Some(p) => std::fs::write(&p, serde_json::to_string_pretty(&doc).unwrap()).expect("write"),
        None => println!("{}", serde_json::to_string_pretty(&doc).unwrap()),
    }
}
