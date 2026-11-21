//! azmatch: plays two searches against each other on the exact engine, everything in sight.
//!
//!   azmatch --a SPEC --b SPEC [--games 100] [--seed 1] [--threads 4] [--map columns-21] [--open 10]
//!
//! SPEC is `flag:N` (the flagship's search, the live playbook's settings, N lines a tick),
//! `pv:NET.json:N` (the network-led search), or either with `Nms` in place of N for a time
//! limit a tick. Each fighter runs its own search every tick it can act, fresh, and plays its
//! most visited turn. Both searches are repeatable, so without help every game would be one of
//! two: each pair of games starts with --open ticks of random steps (no attacks, no dashes),
//! the same steps for both games of the pair, and the two players swap sides between them.

use victris_engine::mcts;
use victris_engine::pvnet::PvNet;
use victris_engine::pvsearch::{self, outcome, PvConfig, Rng};
use victris_engine::state::{FState, State};
use victris_engine::{Engine, RawTurn};

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

enum Limit {
    Lines(u32),
    Ms(f64),
}

struct Player {
    net: Option<PvNet>,
    limit: Limit,
}

fn parse(spec: &str) -> Player {
    let parts: Vec<&str> = spec.split(':').collect();
    let limit = |s: &str| match s.strip_suffix("ms") {
        Some(ms) => Limit::Ms(ms.parse().expect("bad time limit")),
        None => Limit::Lines(s.parse().expect("bad line count")),
    };
    match parts.as_slice() {
        ["flag", l] => Player { net: None, limit: limit(l) },
        ["pv", path, l] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l) },
        _ => panic!("bad player: {spec}"),
    }
}

/// The player's turn for fighter `x` at `s`, the lines it searched and how deep the average line went.
fn think(e: &Engine, p: &Player, flag: mcts::Config, s: &State, x: usize) -> (RawTurn, u64, f64) {
    let started = std::time::Instant::now();
    let mut lines = 0u64;
    match &p.net {
        Some(net) => {
            let mut search = pvsearch::Search::new(e, net, PvConfig::default());
            match p.limit {
                Limit::Lines(n) => {
                    search.iterate(s, n);
                    lines = n as u64;
                }
                Limit::Ms(ms) => {
                    while started.elapsed().as_secs_f64() * 1000.0 < ms {
                        search.iterate(s, 100);
                        lines += 100;
                    }
                }
            }
            let depth: u64 = search.depth_hist.iter().enumerate().map(|(d, n)| d as u64 * n).sum();
            (search.best(x), lines, depth as f64 / lines.max(1) as f64)
        }
        None => {
            let mut search = mcts::Search::new(e, flag);
            search.start();
            match p.limit {
                Limit::Lines(n) => {
                    search.iterate(s, [None, None], n);
                    lines = n as u64;
                }
                Limit::Ms(ms) => {
                    while started.elapsed().as_secs_f64() * 1000.0 < ms {
                        search.iterate(s, [None, None], 100);
                        lines += 100;
                    }
                }
            }
            let r = search.result();
            (r.acts[x].iter().max_by_key(|a| a.1).map(|a| a.0).unwrap_or(RawTurn::IDLE), lines, 0.0)
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (sa, sb): (String, String) = (arg(&args, "--a", String::new()), arg(&args, "--b", String::new()));
    let games: usize = arg(&args, "--games", 100);
    let seed: i64 = arg(&args, "--seed", 1);
    let threads: usize = arg(&args, "--threads", 4);
    let map: String = arg(&args, "--map", String::from("columns-21"));
    let open: i32 = arg(&args, "--open", 10);
    let players = [parse(&sa), parse(&sb)];
    let e = Engine::shipped(&map);
    let flag = mcts::Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior: 0.228, ..Default::default() };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = std::time::Instant::now();
    // per thread: a wins, b wins, draws, a's hp at the end, b's hp at the end, ticks, then per player lines, thinks, depth sum
    let parts: Vec<[f64; 12]> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut st = [0.0f64; 12];
                    loop {
                        let g = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if g >= games {
                            break;
                        }
                        // player a is fighter `side`; games 2k and 2k+1 share a seed with the sides swapped
                        let side = g % 2;
                        let mut s = e.new_match(seed.wrapping_mul(7919).wrapping_add((g / 2) as i64));
                        let mut rng = Rng::new((seed as u64).wrapping_mul(104_729).wrapping_add((g / 2) as u64));
                        let mut buf = Vec::new();
                        while s.result.is_none() {
                            let mut pair = [RawTurn::IDLE, RawTurn::IDLE];
                            for x in 0..2 {
                                if s.tick < open {
                                    victris_engine::pvnet::turns(&e, &s, x, &mut buf);
                                    buf.retain(|t| t.attack == victris_engine::turn::AttackIn::None && t.dash == victris_engine::turn::DirIn::None);
                                    pair[x] = buf[rng.below(buf.len())];
                                } else if s.fighters[x].alive && s.fighters[x].state == FState::Idle {
                                    let who = if x == side { 0 } else { 1 };
                                    let (t, lines, depth) = think(&e, &players[who], flag, &s, x);
                                    pair[x] = t;
                                    st[6 + who * 3] += lines as f64;
                                    st[7 + who * 3] += 1.0;
                                    st[8 + who * 3] += depth;
                                }
                            }
                            e.step(&mut s, &pair);
                        }
                        let z = outcome(&s).unwrap_or(0.0);
                        let a_result = if side == 0 { z } else { -z };
                        st[0] += (a_result > 0.0) as u8 as f64;
                        st[1] += (a_result < 0.0) as u8 as f64;
                        st[2] += (a_result == 0.0) as u8 as f64;
                        st[3] += s.fighters[side].hp.max(0) as f64;
                        st[4] += s.fighters[1 - side].hp.max(0) as f64;
                        st[5] += s.tick as f64;
                    }
                    st
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut st = [0.0f64; 12];
    for p in &parts {
        for i in 0..12 {
            st[i] += p[i];
        }
    }
    let n = (st[0] + st[1] + st[2]).max(1.0);
    let score = (st[0] + 0.5 * st[2]) / n;
    let elo = if score > 0.0 && score < 1.0 { -400.0 * (1.0 / score - 1.0).log10() } else if score >= 1.0 { 999.0 } else { -999.0 };
    let margin = 1.96 * (score * (1.0 - score) / n).sqrt();
    println!(
        "azmatch a={sa} b={sb}: a won {} lost {} drew {} of {} ({:.1}% +-{:.1}, {:+.0} Elo); hp at the end a {:.1} b {:.1}; {:.0} ticks a game; lines a tick a {:.0} b {:.0}; average line a {:.1} b {:.1} ticks; {:.0} s",
        st[0],
        st[1],
        st[2],
        n,
        100.0 * score,
        100.0 * margin,
        elo,
        st[3] / n,
        st[4] / n,
        st[5] / n,
        st[6] / st[7].max(1.0),
        st[9] / st[10].max(1.0),
        st[8] / st[7].max(1.0),
        st[11] / st[10].max(1.0),
        started.elapsed().as_secs_f64()
    );
}
