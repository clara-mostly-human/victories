//! azteach: the teacher on real games. Reads ladder replays and, at the positions of each game,
//! asks the network-led search what it would play and how it rates the position. Writes the
//! games in azgen's format, so the trainer learns from games the ladder really played: the
//! search's shares for the turns, the search's value, and who really won.
//!
//!   azteach --net NET.json --list FILES.txt --out FILE.az [--map columns-21] [--iters 800]
//!           [--every 1] [--threads 4] [--max 1000000]
//!
//! FILES.txt names one replay a line (.jsonl, not gzipped). Skipped: replays of another map
//! or other rules, replays that do not end in a result on the engine (forfeits), and replays
//! with a turn the saved-game format cannot carry exactly (checked by replaying what was
//! saved next to the original, tick by tick). --every K searches every Kth tick only.

use std::io::Write;
use victris_engine::azdata::{shares, GameLog, TickLog};
use victris_engine::pvnet::PvNet;
use victris_engine::pvsearch::{self, outcome, PvConfig};
use victris_engine::replay::parse_replay;
use victris_engine::state::FState;
use victris_engine::{Engine, RawTurn};

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let net_path: String = arg(&args, "--net", String::new());
    let list: String = arg(&args, "--list", String::new());
    let out: String = arg(&args, "--out", String::from("teach.az"));
    let map: String = arg(&args, "--map", String::from("columns-21"));
    let iters: u32 = arg(&args, "--iters", 800);
    let every: i32 = arg::<i32>(&args, "--every", 1).max(1);
    let threads: usize = arg(&args, "--threads", 4);
    let max: usize = arg(&args, "--max", 1_000_000);
    let net = PvNet::load(std::path::Path::new(&net_path)).expect("cannot read the network");
    let files: Vec<String> = std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    let e = Engine::shipped(&map);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let kept = std::sync::atomic::AtomicUsize::new(0);
    let started = std::time::Instant::now();
    // per thread: the games' bytes, then: kept, positions, other map or rules, no result, not carried exactly, unreadable
    let parts: Vec<(Vec<u8>, [u64; 6])> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut bytes: Vec<u8> = Vec::new();
                    let mut st = [0u64; 6];
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if i >= files.len() || kept.load(std::sync::atomic::Ordering::Relaxed) >= max {
                            break;
                        }
                        let Ok(text) = std::fs::read_to_string(&files[i]) else {
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
                        // First the game as it was played, to see that it ends in a result and that
                        // the saved form replays to the same game; only then the searches.
                        let mut log = GameLog { seed: replay.seed, z: 0, ticks: Vec::new() };
                        let mut s = e.new_match(replay.seed);
                        let mut trace = Vec::new();
                        for t in &replay.ticks {
                            if s.result.is_some() {
                                break;
                            }
                            trace.push((s.fighters[0].pos, s.fighters[0].hp, s.fighters[1].pos, s.fighters[1].hp));
                            log.ticks.push(TickLog { turns: t.turns, q: None, pol: None, side: Vec::new() });
                            e.step(&mut s, &t.turns);
                        }
                        let Some(z) = outcome(&s) else {
                            st[3] += 1;
                            continue;
                        };
                        log.z = z as i8;
                        let mut probe = Vec::new();
                        log.write(&mut probe);
                        let back = GameLog::read_all(&probe);
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
                        // The searches: a fresh one at each chosen tick where a fighter can act.
                        let mut s = e.new_match(replay.seed);
                        for k in 0..log.ticks.len() {
                            let free = |x: usize| s.fighters[x].alive && s.fighters[x].state == FState::Idle;
                            if (free(0) || free(1)) && s.tick % every == 0 {
                                let mut search = pvsearch::Search::new(&e, &net, PvConfig::default());
                                search.iterate(&s, iters);
                                let r = search.root();
                                let visits: [Vec<(RawTurn, u32)>; 2] = [r[0].iter().map(|a| (a.0, a.1)).collect(), r[1].iter().map(|a| (a.0, a.1)).collect()];
                                log.ticks[k].q = Some(search.value());
                                log.ticks[k].pol = Some([shares(&visits[0]), shares(&visits[1])]);
                                st[1] += 1;
                            }
                            let turns = log.ticks[k].turns;
                            e.step(&mut s, &turns);
                        }
                        log.write(&mut bytes);
                        st[0] += 1;
                        kept.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        file.write_all(bytes).unwrap();
        for i in 0..6 {
            st[i] += s[i];
        }
    }
    file.flush().unwrap();
    println!(
        "azteach: {} of {} replays kept, {} positions searched at {} lines; skipped: {} other map or rules, {} without a result, {} not carried exactly, {} unreadable; {:.0} s",
        st[0],
        files.len(),
        st[1],
        iters,
        st[2],
        st[3],
        st[4],
        st[5],
        started.elapsed().as_secs_f64()
    );
}
