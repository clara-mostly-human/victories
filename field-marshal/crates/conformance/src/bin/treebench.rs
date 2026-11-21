//! treebench: how fast is the tree search, and where does the time go?
//!
//!   treebench <replay.jsonl.gz> [--tick 100] [--iterations 30000]
//!
//! Plays the replay to the given tick and searches that position with a few settings,
//! printing iterations a second for each: the search as played, without the judgment,
//! with shorter play-outs at the leaf, and at smaller depths.

use flate2::read::GzDecoder;
use std::io::Read;
use std::time::Instant;
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::parse_replay;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let tick: usize = opt("--tick").and_then(|s| s.parse().ok()).unwrap_or(100);
    let iterations: u32 = opt("--iterations").and_then(|s| s.parse().ok()).unwrap_or(30_000);
    let path = args.iter().find(|a| a.ends_with(".jsonl.gz")).expect("a replay file");
    let mut text = String::new();
    GzDecoder::new(std::fs::File::open(path).expect("open")).read_to_string(&mut text).expect("read");
    let replay = parse_replay(&text).expect("parse");
    let e = &replay.engine;
    let mut s = e.new_match(replay.seed);
    for t in replay.ticks.iter().take(tick) {
        e.step(&mut s, &t.turns);
    }
    println!("one turn in the tree takes {} bytes", std::mem::size_of::<victris_engine::mcts::Act>());
    println!("{} tick {}: hp {} and {}, {} tiles apart", e.map.name, s.tick, s.fighters[0].hp, s.fighters[1].hp, victris_engine::geom::chebyshev(s.fighters[0].pos, s.fighters[1].pos));
    let base = Config { value_weight: 1.0, value_gen: 1, buff_value: 0.0, ..Default::default() };
    let variants: Vec<(&str, Config)> = vec![
        ("as played (judgment 1, play-out 6, depth 12)", base),
        ("no judgment", Config { value_weight: 0.0, buff_value: 0.3, ..base }),
        ("play-out 3", Config { quiet: 3, ..base }),
        ("play-out 0", Config { quiet: 0, ..base }),
        ("depth 6", Config { depth: 6, ..base }),
        ("depth 3", Config { depth: 3, ..base }),
        ("judgment with 31 features (candidate 4)", Config { value_gen: 4, ..base }),
        ("expand on the second visit", Config { expand_at: 1, ..base }),
        ("expand on the second visit, dash + repulsor", Config { expand_at: 1, dash_burst: true, ..base }),
        ("dash + repulsor", Config { dash_burst: true, ..base }),
        ("flagship (row 13, dash + repulsor)", Config { value_gen: 13, dash_burst: true, ..base }),
        ("flagship + slam prior 0.228", Config { value_gen: 13, dash_burst: true, slam_prior: 0.228, ..base }),
        ("win-probability judgment only (40)", Config { value_weight: 0.0, net_weight: 40.0, dash_burst: true, ..base }),
    ];
    for (name, cfg) in variants {
        let mut search = Search::new(e, cfg);
        search.start();
        let started = Instant::now();
        search.iterate(&s, [None, None], iterations);
        let secs = started.elapsed().as_secs_f64();
        let root = search.result();
        let best = |x: usize| root.acts[x].iter().max_by_key(|a| a.1).map(|a| (a.1, a.2)).unwrap_or((0, 0.0));
        for x in 0..2 {
            let mv = root.acts[x].iter().max_by_key(|a| a.1).map(|a| a.0);
            let sv = search.root_solve(x, 6, 2000);
            let q = |t: Option<victris_engine::turn::RawTurn>| t.and_then(|t| root.acts[x].iter().find(|a| a.0 == t)).map(|a| (a.1, a.2));
            println!("      fighter {x}: most visited {:?} -> {:?}; solve -> {:?}", mv.map(|t| (t.mv, t.dash, t.attack)), q(mv), q(sv));
        }
        println!("  {:<46} {:>7.0} iterations/s, {:>6} nodes, {:>5.1} MB of move lists   root value {:+.1}, top turn share {:.0}% and {:.0}%, {} and {} turns", name, iterations as f64 / secs, search.node_count(), search.list_bytes() as f64 / 1048576.0, root.value,
            100.0 * best(0).0 as f64 / iterations as f64, 100.0 * best(1).0 as f64 / iterations as f64, root.acts[0].len(), root.acts[1].len());
    }
}
