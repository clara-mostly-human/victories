//! probe: what the flagship's search makes of one position from a replay.
//!
//!   probe <replay> --bot NAME --ticks 19,20,21 [--iterations 7500] [--slam-prior 0.228] [--top 6]
//!
//! For each tick, searches the position from the named bot's side the way the flagship does
//! (judgment row 13, dash + repulsor) and prints its most visited turns with their values, the
//! opponent's most visited turns, and the turns both actually played.

use flate2::read::GzDecoder;
use std::io::Read;
use victris_engine::geom::DIR_NAMES;
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::turn::{AttackIn, DirIn, RawTurn};
use victris_engine::Engine;

fn describe(e: &Engine, t: &RawTurn) -> String {
    let mut out = Vec::new();
    if let DirIn::Dir(d) = t.mv {
        out.push(format!("step {}", DIR_NAMES[d as usize]));
    }
    if let DirIn::Dir(d) = t.dash {
        out.push(format!("dash {}", DIR_NAMES[d as usize]));
    }
    if let AttackIn::Named { attack, .. } = t.attack {
        out.push(e.rules.attacks[attack as usize].name.clone());
    }
    if out.is_empty() {
        "hold".into()
    } else {
        out.join(" + ")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let path = args.iter().find(|a| a.ends_with(".jsonl.gz") || a.ends_with(".jsonl")).expect("a replay");
    let bot = opt("--bot").expect("--bot NAME");
    let ticks: Vec<i32> = opt("--ticks").map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_default();
    let iterations: u32 = opt("--iterations").and_then(|s| s.parse().ok()).unwrap_or(7500);
    let slam_prior: f32 = opt("--slam-prior").and_then(|s| s.parse().ok()).unwrap_or(0.228);
    let deadline_k: usize = opt("--deadline").and_then(|s| s.parse().ok()).unwrap_or(0);
    let deadline_ext: i32 = opt("--ext").and_then(|s| s.parse().ok()).unwrap_or(0);
    let top: usize = opt("--top").and_then(|s| s.parse().ok()).unwrap_or(6);
    let mut text = String::new();
    if path.ends_with(".gz") {
        GzDecoder::new(std::fs::File::open(path).expect("open")).read_to_string(&mut text).expect("read");
    } else {
        text = std::fs::read_to_string(path).expect("read");
    }
    let replay = parse_replay(&text).expect("parse");
    let me = replay.bots.iter().position(|b| *b == bot).expect("bot in replay");
    let e = &replay.engine;
    let mut s = e.new_match(replay.seed);
    let cfg = Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior, deadline_k, deadline_ext, ..Default::default() };
    for t in &replay.ticks {
        if s.result.is_some() {
            break;
        }
        if std::env::var("PROBE_DEBUG").is_ok() && (18..=23).contains(&s.tick) {
            eprintln!("tick {} spawn_fight(3) {} spawn_due {} pads {:?}", s.tick, victris_engine::mcts::in_spawn_fight(e, &s, 3), victris_engine::mcts::spawn_due(e, &s), s.pads.iter().take(e.map.pads.len()).map(|p| (p.available, p.respawn_tick)).collect::<Vec<_>>());
        }
        if ticks.contains(&s.tick) {
            let mut search = Search::new(e, cfg);
            search.start();
            search.iterate(&s, [None, None], iterations);
            let r = search.result();
            println!("tick {} ({} at {:?}, opponent at {:?}), root value {:+.1}", s.tick, bot, s.fighters[me].pos, s.fighters[1 - me].pos, r.value);
            for (x, who) in [(me, "us"), (1 - me, "them")] {
                let tot: u32 = r.acts[x].iter().map(|a| a.1).sum();
                let mut v: Vec<_> = r.acts[x].clone();
                v.sort_by(|a, b| b.1.cmp(&a.1));
                let played = t.turns[x];
                let pr = v.iter().position(|a| a.0 == played);
                println!(
                    "  {who}: {} | played {} (rank {})",
                    v.iter().take(top).map(|a| format!("{} {:.0}% {:+.1}", describe(e, &a.0), 100.0 * a.1 as f64 / tot.max(1) as f64, a.2)).collect::<Vec<_>>().join(", "),
                    describe(e, &played),
                    pr.map(|p| (p + 1).to_string()).unwrap_or_else(|| "not in list".into())
                );
            }
        }
        e.step(&mut s, &t.turns);
    }
}
