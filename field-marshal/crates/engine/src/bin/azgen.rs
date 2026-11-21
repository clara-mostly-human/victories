//! azgen: self-play that saves every search's conclusion.
//!
//! Both fighters are played by one search on the exact engine, with everything in sight. At every
//! tick it saves the position, where the search put its visits for each fighter, the search's
//! value, and (when the game ends) who won. `aztrain` learns from these files.
//!
//!   azgen --out FILE --games N [--net NET.json] [--iters 800] [--seed 1] [--threads 4]
//!         [--map columns-21] [--wander 0.25] [--open 12] [--wide 0.25]
//!
//! With --net the search is `pvsearch` led by that network. Without it the search is the
//! flagship's (`mcts`, the live playbook's settings): the first generation learns from the
//! search we already have, the following ones from their own play.

use std::io::Write;
use victris_engine::mcts;
use victris_engine::pvnet::{PvNet, Rec, REC};
use victris_engine::pvsearch::{self, outcome, pick, record, PvConfig, Rng};
use victris_engine::state::FState;
use victris_engine::{Engine, RawTurn};

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out: String = arg(&args, "--out", String::from("selfplay.az"));
    let games: usize = arg(&args, "--games", 100);
    let iters: u32 = arg(&args, "--iters", 800);
    let seed: u64 = arg(&args, "--seed", 1);
    let threads: usize = arg(&args, "--threads", 4);
    let map: String = arg(&args, "--map", String::from("columns-21"));
    let wander: f64 = arg(&args, "--wander", 0.25);
    let open: i32 = arg(&args, "--open", 12);
    let wide: f32 = arg(&args, "--wide", 0.25);
    let net_path: String = arg(&args, "--net", String::new());
    let net = if net_path.is_empty() { None } else { Some(PvNet::load(std::path::Path::new(&net_path)).expect("cannot read the network")) };
    let e = Engine::shipped(&map);
    let flag = mcts::Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior: 0.228, ..Default::default() };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = std::time::Instant::now();
    let parts: Vec<(Vec<u8>, [u64; 6])> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut bytes: Vec<u8> = Vec::new();
                    // games, positions, ticks, fighter 0 wins, fighter 1 wins, search lines
                    let mut st = [0u64; 6];
                    loop {
                        let g = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if g >= games {
                            break;
                        }
                        let mut rng = Rng::new(seed.wrapping_mul(1_000_003).wrapping_add(g as u64));
                        let mut s = e.new_match((seed as i64).wrapping_mul(7919).wrapping_add(g as i64));
                        let mut recs: Vec<Rec> = Vec::new();
                        let mut pv = net.as_ref().map(|n| pvsearch::Search::new(&e, n, PvConfig { even_root: wide, ..Default::default() }));
                        while s.result.is_none() {
                            let free = |x: usize| s.fighters[x].alive && s.fighters[x].state == FState::Idle;
                            if !free(0) && !free(1) {
                                e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
                                if let Some(p) = pv.as_mut() {
                                    p.advance(&RawTurn::IDLE, &RawTurn::IDLE);
                                }
                                continue;
                            }
                            let (visits, q): ([Vec<(RawTurn, u32)>; 2], Option<f32>) = match pv.as_mut() {
                                Some(p) => {
                                    p.iterate(&s, iters);
                                    let r = p.root();
                                    ([r[0].iter().map(|a| (a.0, a.1)).collect(), r[1].iter().map(|a| (a.0, a.1)).collect()], Some(p.value()))
                                }
                                None => {
                                    let mut search = mcts::Search::new(&e, flag);
                                    search.start();
                                    search.iterate(&s, [None, None], iters);
                                    let r = search.result();
                                    ([r.acts[0].iter().map(|a| (a.0, a.1)).collect(), r.acts[1].iter().map(|a| (a.0, a.1)).collect()], None)
                                }
                            };
                            st[5] += iters as u64;
                            recs.push(record(&e, &s, [&visits[0], &visits[1]], q));
                            let w = if s.tick < open { 1.0 } else { wander };
                            let pair = [pick(&visits[0], w, &mut rng), pick(&visits[1], w, &mut rng)];
                            e.step(&mut s, &pair);
                            if let Some(p) = pv.as_mut() {
                                p.advance(&pair[0], &pair[1]);
                            }
                        }
                        let z = outcome(&s).unwrap_or(0.0) as i8;
                        st[0] += 1;
                        st[1] += recs.len() as u64;
                        st[2] += s.tick.max(0) as u64;
                        st[3] += (z > 0) as u64;
                        st[4] += (z < 0) as u64;
                        for r in recs.iter_mut() {
                            r.z = z;
                            r.write(&mut bytes);
                        }
                    }
                    (bytes, st)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out).expect("cannot write the output"));
    let mut st = [0u64; 6];
    for (bytes, s) in &parts {
        debug_assert_eq!(bytes.len() % REC, 0);
        file.write_all(bytes).unwrap();
        for i in 0..6 {
            st[i] += s[i];
        }
    }
    file.flush().unwrap();
    let secs = started.elapsed().as_secs_f64();
    println!(
        "azgen {}: {} games, {} positions, {:.0} ticks a game, fighter 0 won {} / fighter 1 won {} / drawn {}, {:.1} s ({:.1} games a minute, {:.0} lines a second a thread)",
        if net.is_some() { "network" } else { "flagship" },
        st[0],
        st[1],
        st[2] as f64 / st[0].max(1) as f64,
        st[3],
        st[4],
        st[0] - st[3] - st[4],
        secs,
        st[0] as f64 / secs * 60.0,
        st[5] as f64 / secs / threads as f64
    );
}
