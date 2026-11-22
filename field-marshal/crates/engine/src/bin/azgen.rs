//! azgen: self-play that saves every search's conclusion.
//!
//! Both fighters are played by one search on the exact engine, with everything in sight. At every
//! tick it saves the turns played, where the search put its visits for each fighter and the
//! search's value, and (when the game ends) who won (`azdata`). `aztrain` learns from these
//! files.
//!
//!   azgen --out FILE --games N [--net NET.json] [--iters 800] [--seed 1] [--threads 4]
//!         [--map columns-21] [--wander 0.25] [--open 12] [--wide 0.25] [--earned 0] [--mix 0]
//!         [--lead 0] [--side 0[:64]]
//!
//! Which turn is played: before tick --open one drawn in proportion to the search's visits;
//! after it the most visited one, or with chance --wander a drawn one. That plays a turn the
//! search gave a third of its visits about one time in twelve: where the right play is a
//! mix (both move at once), self-play then carries a threat out far less often than its own
//! search thinks right, and the network never learns what the threat costs. With --mix 1
//! (the network-led search only) both fighters' turns are drawn at every tick in proportion
//! to the visits each turn earned (`Search::root_earned`: the search's own mix, without the
//! visits the wider root alone gave), so every turn is played as often as the search rates it.
//!
//! --wide makes the search look wider at the root than anywhere else, so self-play tries turns
//! the network thinks little of. With --earned 1 what is saved for the trainer is the visits
//! each turn earned (`Search::root_earned`), without the ones that width alone gave it; the
//! turns played are still picked from the plain visits.
//!
//! With --lead N (the network-led search only) the search may lead its shots, and expects the
//! other fighter to, from N lines through the root on (`Search::lead`).
//!
//! With --side K (the network-led search only) every searched tick also saves K positions
//! beside the game's path: positions the search tried from the tick's position but the game
//! did not go to, each looked at until 64 lines (--side K:LINES for another count) began with
//! it, with the search's value of it (`Search::beside`). The trainer learns their value. The
//! games played are the same games as without it.
//!
//! With --net the search is `pvsearch` led by that network. Without it the search is the
//! flagship's (`mcts`, the live playbook's settings): the first generation learns from the
//! search we already have, the following ones from their own play.

use std::io::Write;
use victris_engine::mcts;
use victris_engine::azdata::{shares, shares_earned, GameLog, SideLog, TickLog};
use victris_engine::pvnet::{PvNet, CLASSES};
use victris_engine::pvsearch::{self, outcome, pick, PvConfig, Rng};
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
    let earned: u32 = arg(&args, "--earned", 0);
    let mix: u32 = arg(&args, "--mix", 0);
    let lead: u32 = arg(&args, "--lead", 0);
    let side: String = arg(&args, "--side", String::from("0"));
    let (side_count, side_lines): (usize, u32) = match side.split_once(':') {
        Some((k, l)) => (k.parse().expect("--side K:LINES"), l.parse().expect("--side K:LINES")),
        None => (side.parse().expect("--side K"), 64),
    };
    assert!(side_count <= 255, "--side: at most 255 positions a tick");
    let net_path: String = arg(&args, "--net", String::new());
    let net = if net_path.is_empty() { None } else { Some(PvNet::load(std::path::Path::new(&net_path)).expect("cannot read the network")) };
    let e = Engine::shipped(&map);
    let flag = mcts::Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior: 0.228, ..Default::default() };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = std::time::Instant::now();
    let parts: Vec<(Vec<u8>, [u64; 8])> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut bytes: Vec<u8> = Vec::new();
                    // games, positions, ticks, fighter 0 wins, fighter 1 wins, search lines,
                    // positions beside the path, the lines behind those
                    let mut st = [0u64; 8];
                    loop {
                        let g = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if g >= games {
                            break;
                        }
                        let mut rng = Rng::new(seed.wrapping_mul(1_000_003).wrapping_add(g as u64));
                        // the draws of positions beside the path, apart from the game's own draws
                        let mut rng_side = Rng::new(seed.wrapping_mul(2_000_003).wrapping_add(g as u64) ^ 0x51DE);
                        let mut s = e.new_match((seed as i64).wrapping_mul(7919).wrapping_add(g as i64));
                        let match_seed = (seed as i64).wrapping_mul(7919).wrapping_add(g as i64);
                        let mut log = GameLog { seed: match_seed, z: 0, ticks: Vec::new() };
                        let mut searched = 0u64;
                        let mut pv = net.as_ref().map(|n| pvsearch::Search::new(&e, n, PvConfig { even_root: wide, lead_at: lead, ..Default::default() }));
                        while s.result.is_none() {
                            let free = |x: usize| s.fighters[x].alive && s.fighters[x].state == FState::Idle;
                            if !free(0) && !free(1) {
                                e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
                                log.ticks.push(TickLog { turns: [RawTurn::IDLE, RawTurn::IDLE], q: None, pol: None, side: Vec::new() });
                                if let Some(p) = pv.as_mut() {
                                    p.advance(&RawTurn::IDLE, &RawTurn::IDLE);
                                }
                                continue;
                            }
                            let mut pol: Option<[[u8; CLASSES]; 2]> = None;
                            // with --mix: what each turn earned, to draw the turns played from
                            let mut drawn: Option<[Vec<(RawTurn, u32)>; 2]> = None;
                            let (visits, q): ([Vec<(RawTurn, u32)>; 2], Option<f32>) = match pv.as_mut() {
                                Some(p) => {
                                    p.iterate(&s, iters);
                                    let r = p.root();
                                    if earned > 0 || mix > 0 {
                                        let k = p.root_earned();
                                        if earned > 0 {
                                            pol = Some([shares_earned(&k[0]), shares_earned(&k[1])]);
                                        }
                                        if mix > 0 {
                                            let whole = |x: usize| k[x].iter().map(|a| (a.0, a.1.round() as u32)).collect::<Vec<_>>();
                                            drawn = Some([whole(0), whole(1)]);
                                        }
                                    }
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
                            searched += 1;
                            let w = if s.tick < open { 1.0 } else { wander };
                            let pair = match &drawn {
                                Some(d) => [pick(&d[0], 1.0, &mut rng), pick(&d[1], 1.0, &mut rng)],
                                None => [pick(&visits[0], w, &mut rng), pick(&visits[1], w, &mut rng)],
                            };
                            let mut beside: Vec<SideLog> = Vec::new();
                            if side_count > 0 {
                                if let Some(p) = pv.as_mut() {
                                    for (turns, q, lines) in p.beside(&s, &pair, side_count, side_lines, &mut rng_side) {
                                        st[6] += 1;
                                        st[7] += lines as u64;
                                        beside.push(SideLog { turns, q, lines });
                                    }
                                }
                            }
                            log.ticks.push(TickLog { turns: pair, q, pol: Some(pol.unwrap_or_else(|| [shares(&visits[0]), shares(&visits[1])])), side: beside });
                            e.step(&mut s, &pair);
                            if let Some(p) = pv.as_mut() {
                                p.advance(&pair[0], &pair[1]);
                            }
                        }
                        let z = outcome(&s).unwrap_or(0.0) as i8;
                        st[0] += 1;
                        st[1] += searched;
                        st[2] += s.tick.max(0) as u64;
                        st[3] += (z > 0) as u64;
                        st[4] += (z < 0) as u64;
                        log.z = z;
                        log.write(&mut bytes);
                    }
                    (bytes, st)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out).expect("cannot write the output"));
    let mut st = [0u64; 8];
    for (bytes, s) in &parts {
        file.write_all(bytes).unwrap();
        for i in 0..8 {
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
    if st[6] > 0 {
        println!("  beside the games: {} positions, {:.0} lines behind each", st[6], st[7] as f64 / st[6] as f64);
    }
}
