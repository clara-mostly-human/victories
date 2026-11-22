//! azreal: real ladder games as lessons in who wins.
//!
//!   azreal --list FILES.txt --out FILE.az [--map columns-21]
//!
//! Self-play teaches the network what a position is worth between two copies of our own
//! search. Against the ladder some positions are worth something else: checked against 1,448
//! real games on columns-21 (`azvalue`), a self-play network was too hopeful by 0.045 where the
//! other fighter held its dash and repulsor ready within 4 tiles, and by 0.084 where it was
//! powered as well; in self-play games the same check showed no gap. Self-play's opponent does
//! not make that threat pay, so nothing in self-play can teach what it costs.
//!
//! This writes every position of every replay in FILES.txt (one a line, .jsonl, not gzipped)
//! in azgen's format with the game's real result and no shares for the turns: handed to
//! `aztrain` beside the self-play games, such a position teaches the value and leaves the moves
//! alone (a lesson without shares has no moves loss). The moves are still learned from
//! self-play only; real games say how positions turn out, not how to play them.
//!
//! Skipped: replays of another map or other rules, replays that do not end in a result on the
//! engine (forfeits), and replays with a turn the saved-game format cannot carry exactly
//! (checked by replaying what was saved next to the original, tick by tick).

use victris_engine::azdata::{GameLog, TickLog};
use victris_engine::pvnet::CLASSES;
use victris_engine::pvsearch::outcome;
use victris_engine::replay::parse_replay;
use victris_engine::Engine;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let list: String = arg(&args, "--list", String::new());
    let out: String = arg(&args, "--out", String::from("real.az"));
    let map: String = arg(&args, "--map", String::from("columns-21"));
    let e = Engine::shipped(&map);
    let files: Vec<String> = std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    let mut bytes = Vec::new();
    // kept, positions, other map or rules, no result, not carried exactly, unreadable
    let mut st = [0u64; 6];
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            st[5] += 1;
            continue;
        };
        let Ok(replay) = parse_replay(&text) else {
            st[5] += 1;
            continue;
        };
        if replay.engine.map.name != e.map.name || replay.engine.rules.hash != e.rules.hash {
            st[2] += 1;
            continue;
        }
        let mut log = GameLog { seed: replay.seed, z: 0, ticks: Vec::new() };
        let mut s = e.new_match(replay.seed);
        let mut trace = Vec::new();
        let mut positions = 0u64;
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            trace.push((s.fighters[0].pos, s.fighters[0].hp, s.fighters[1].pos, s.fighters[1].hp));
            // a lesson with no shares: the position and, from the game, who won
            let both = s.fighters[0].alive && s.fighters[1].alive;
            positions += both as u64;
            log.ticks.push(TickLog { turns: t.turns, q: None, pol: if both { Some([[0u8; CLASSES]; 2]) } else { None }, side: Vec::new() });
            e.step(&mut s, &t.turns);
        }
        let Some(z) = outcome(&s) else {
            st[3] += 1;
            continue;
        };
        log.z = z as i8;
        let mut saved = Vec::new();
        log.write(&mut saved);
        let back = GameLog::read_all(&saved);
        let mut s2 = e.new_match(replay.seed);
        let mut same = back.len() == 1 && back[0].ticks.len() == log.ticks.len();
        if same {
            for (k, t) in back[0].ticks.iter().enumerate() {
                if trace[k] != (s2.fighters[0].pos, s2.fighters[0].hp, s2.fighters[1].pos, s2.fighters[1].hp) {
                    same = false;
                    break;
                }
                e.step(&mut s2, &t.turns);
            }
            same = same && outcome(&s2) == Some(z);
        }
        if !same {
            st[4] += 1;
            continue;
        }
        bytes.extend_from_slice(&saved);
        st[0] += 1;
        st[1] += positions;
    }
    std::fs::write(&out, &bytes).expect("cannot write the lessons");
    println!(
        "azreal {map}: {} games kept, {} positions; skipped: {} of another map or rules, {} without a result, {} not carried exactly, {} unreadable -> {out}",
        st[0], st[1], st[2], st[3], st[4], st[5]
    );
}
