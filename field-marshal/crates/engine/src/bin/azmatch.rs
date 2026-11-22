//! azmatch: plays two searches against each other on the exact engine, everything in sight.
//!
//!   azmatch --a SPEC --b SPEC [--games 100] [--seed 1] [--threads 4] [--map columns-21] [--open 10]
//!           [--book GAMES.az] [--soft 0]
//!
//! SPEC is `flag:N` (the flagship's search, the live playbook's settings, N lines a tick),
//! `pv:NET.json:N` (the network-led search), `pvw:NET.json:N[:SHARE[:TRUST]]` (the same, its
//! turn chosen by `Search::weigh` with SHARE of the N lines, a fifth unless given, spent
//! weighing, and TRUST, 1 unless given, in the turns expected of the opponent),
//! `pvd:NET.json:N:K` (the network-led search with K times the lines at a standoff: what a
//! memory of that one situation, searched deep beforehand, could be worth),
//! `pvt:NET.json:N:K` (the network-led search on K threads sharing its tree,
//! `Search::iterate_threads`; against `pv` at the same N it shows what the sharing costs a line), or
//! `flag`/`pv` with `Nms` in place of N for a time limit a tick. Each fighter runs its own search every tick it can act, fresh, and plays its
//! most visited turn. Both searches are repeatable, so without help every game would be one of
//! two: each pair of games starts with --open ticks of random steps (no attacks, no dashes),
//! the same steps for both games of the pair, and the two players swap sides between them.
//! Those openings are not enough: nothing has happened yet, so many games settle into the same
//! few games anyway. With --book each pair instead starts from a position out of a saved game
//! (azgen's file): a random tick from 20 to 220 with both alive and the hp within 20.
//! --soft K tests play from the spawn instead: before tick K each player draws its turn in
//! proportion to its own search's visits, so every game is different and each player still
//! plays its own opening (use with --open 0).

use victris_engine::azdata::GameLog;
use victris_engine::mcts;
use victris_engine::pvnet::PvNet;
use victris_engine::pvsearch::{self, outcome, pick, PvConfig, Rng};
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
    /// share of the lines spent weighing our turns against theirs (`Search::weigh`); 0: off
    weigh: f64,
    trust: f32,
    /// the lines are multiplied by this at a standoff (`standoff`); 1: no difference
    deep: u32,
    /// threads of one search (`Search::iterate_threads`); 1: the plain search
    search_threads: usize,
}

/// The commonest situation of real arena-21 games: the fighters four tiles apart, both free
/// with the dash and every attack ready.
fn standoff(e: &Engine, s: &State) -> bool {
    let n = e.rules.attacks.len();
    let ready = |f: &victris_engine::state::Fighter| f.alive && f.state == FState::Idle && f.cd_dash == 0 && f.cd_attack[..n].iter().all(|c| *c == 0);
    let (a, b) = (&s.fighters[0], &s.fighters[1]);
    ready(a) && ready(b) && (a.pos[0] - b.pos[0]).abs().max((a.pos[1] - b.pos[1]).abs()) == 4
}

fn parse(spec: &str) -> Player {
    let parts: Vec<&str> = spec.split(':').collect();
    let limit = |s: &str| match s.strip_suffix("ms") {
        Some(ms) => Limit::Ms(ms.parse().expect("bad time limit")),
        None => Limit::Lines(s.parse().expect("bad line count")),
    };
    match parts.as_slice() {
        ["flag", l] => Player { net: None, limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: 1 },
        ["pv", path, l] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: 1 },
        ["pvd", path, l, k] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: k.parse().expect("bad multiple"), search_threads: 1 },
        ["pvt", path, l, k] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: k.parse().expect("bad thread count") },
        ["pvw", path, l, rest @ ..] if rest.len() <= 2 => Player {
            net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")),
            limit: limit(l),
            weigh: rest.first().map(|w| w.parse().expect("bad share")).unwrap_or(0.2),
            trust: rest.get(1).map(|w| w.parse().expect("bad trust")).unwrap_or(1.0),
            deep: 1,
            search_threads: 1,
        },
        _ => panic!("bad player: {spec}"),
    }
}

/// The player's turn for fighter `x` at `s`, the lines it searched and how deep the average line went.
fn think(e: &Engine, p: &Player, flag: mcts::Config, s: &State, x: usize) -> (RawTurn, u64, f64, Vec<(RawTurn, u32)>) {
    let started = std::time::Instant::now();
    let mut lines = 0u64;
    match &p.net {
        Some(net) => {
            let mut search = pvsearch::Search::new(e, net, PvConfig::default());
            let mut held = 0u32;
            match p.limit {
                Limit::Lines(n) => {
                    let n = if p.deep > 1 && standoff(e, s) { n * p.deep } else { n };
                    held = (n as f64 * p.weigh).round() as u32;
                    search.iterate_threads(s, n - held, p.search_threads, None);
                    lines = n as u64;
                }
                Limit::Ms(ms) if p.search_threads > 1 => {
                    lines = search.iterate_threads(s, u32::MAX, p.search_threads, Some(started + std::time::Duration::from_secs_f64(ms * (1.0 - p.weigh) / 1000.0))) as u64;
                }
                Limit::Ms(ms) => {
                    while started.elapsed().as_secs_f64() * 1000.0 < ms * (1.0 - p.weigh) {
                        search.iterate(s, 100);
                        lines += 100;
                    }
                    if p.weigh > 0.0 {
                        held = (lines as f64 * p.weigh / (1.0 - p.weigh)).round() as u32;
                        lines += held as u64;
                    }
                }
            }
            let visits: Vec<(RawTurn, u32)> = search.root()[x].iter().map(|v| (v.0, v.1)).collect();
            let turn = if p.weigh > 0.0 { search.weigh(s, x, held.max(1), p.trust)[0].turn } else { search.best(x) };
            let depth: u64 = search.depth_hist.iter().enumerate().map(|(d, n)| d as u64 * n).sum();
            (turn, lines, depth as f64 / lines.max(1) as f64, visits)
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
            let visits: Vec<(RawTurn, u32)> = r.acts[x].iter().map(|a| (a.0, a.1)).collect();
            (r.acts[x].iter().max_by_key(|a| a.1).map(|a| a.0).unwrap_or(RawTurn::IDLE), lines, 0.0, visits)
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
    // AZ_GAMES=1 prints a line for each game
    let list = std::env::var("AZ_GAMES").is_ok();
    let soft: i32 = arg(&args, "--soft", 0);
    let book_path: String = arg(&args, "--book", String::new());
    let book: Vec<GameLog> = if book_path.is_empty() { Vec::new() } else { GameLog::read_all(&std::fs::read(&book_path).expect("cannot read the book")) };
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
                        if !book.is_empty() {
                            // a position out of a saved game, the same one for both games of the pair
                            let mut seen = Vec::new();
                            for _ in 0..20 {
                                seen.clear();
                                if !book[rng.below(book.len())].samples(&e, &mut seen) {
                                    continue;
                                }
                                let fit: Vec<usize> = (0..seen.len())
                                    .filter(|i| {
                                        let t = &seen[*i].s;
                                        t.tick >= 20 && t.tick <= 220 && t.fighters[0].alive && t.fighters[1].alive && (t.fighters[0].hp - t.fighters[1].hp).abs() <= 20
                                    })
                                    .collect();
                                if !fit.is_empty() {
                                    s = seen[fit[rng.below(fit.len())]].s;
                                    break;
                                }
                            }
                        }
                        let mut buf = Vec::new();
                        // this game's own draws, for --soft
                        let mut own = Rng::new((seed as u64).wrapping_mul(15_485_863).wrapping_add(g as u64));
                        while s.result.is_none() {
                            let mut pair = [RawTurn::IDLE, RawTurn::IDLE];
                            for x in 0..2 {
                                if s.tick < open {
                                    victris_engine::pvnet::turns(&e, &s, x, &mut buf);
                                    buf.retain(|t| t.attack == victris_engine::turn::AttackIn::None && t.dash == victris_engine::turn::DirIn::None);
                                    pair[x] = buf[rng.below(buf.len())];
                                } else if s.fighters[x].alive && s.fighters[x].state == FState::Idle {
                                    let who = if x == side { 0 } else { 1 };
                                    let (t, lines, depth, visits) = think(&e, &players[who], flag, &s, x);
                                    pair[x] = if s.tick < soft { pick(&visits, 1.0, &mut own) } else { t };
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
                        if list {
                            println!("game {g} side {side} ticks {} hp a {} b {} a's result {a_result}", s.tick, s.fighters[side].hp.max(0), s.fighters[1 - side].hp.max(0));
                        }
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
