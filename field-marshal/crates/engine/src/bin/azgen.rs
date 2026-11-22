//! azgen: self-play that saves every search's conclusion.
//!
//! Both fighters are played by one search on the exact engine, with everything in sight. At every
//! tick it saves the turns played, where the search put its visits for each fighter and the
//! search's value, and (when the game ends) who won (`azdata`). `aztrain` learns from these
//! files.
//!
//!   azgen --out FILE --games N [--net NET.json] [--iters 800] [--seed 1] [--threads 4]
//!         [--map columns-21] [--wander 0.25] [--open 12] [--wide 0.25] [--earned 0] [--mix 0]
//!         [--lead 0] [--from GAMES.az] [--from-share 0.5] [--from-tick 0]
//!         [--from-level 1000] [--read 0] [--read-lines N] [--read-teach 0] [--teach-answer 0]
//!
//! With --from a share of the games (--from-share) do not start at tick 0 but from a position
//! out of a saved game (ladder games as `azreal` saves them): a game drawn from the file is
//! replayed to a tick drawn evenly from --from-tick up to five ticks before its end, and the
//! search plays both fighters on from there. Self-play on its own only ever learns the
//! positions its own play reaches; ladder opponents take the bot to others (the small last
//! zone, an opponent that keeps away while we hold a power pad). With --from-level H only
//! positions with the two fighters' hp within H are taken, so the game that follows is long.
//! The replayed ticks are saved with no lesson, so the trainer replays the whole game and
//! learns from the searched ticks only.
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
//! With --read P (the network-led search only), at each tick with chance P one of the two
//! fighters, drawn evenly, is told the turn the other has just chosen and plays its best
//! answer to it (`Search::answer`, --read-lines more lines, half of --iters unless given).
//! An opponent on the ladder that guesses our turn right gets the same: it slams the
//! fighter that stands on the edge, shoots the tile the slowed fighter steps to. Our own
//! search carries such a threat out only as often as a mix asks, so the network learns the
//! position is cheap. Here a habit that can be read is punished as soon as it is shown,
//! and the game's result teaches what the position costs. What is saved for the trainer
//! at that tick is still the plain search's visits for both fighters: no bot is told.
//! With --read-teach 1 the fighter that was told is taught its answer instead: the shares of
//! the lines each of its turns got against the told turn (less the one line every turn is
//! given to begin with). Its lesson is then the best play against what the other fighter
//! really does in that position, not against everything the search thought it might do: a
//! network taught so comes to expect the punishment of its own habits, and its search stops
//! having them (each side answering what the other has been doing settles on the mix).
//! With --teach-answer P the same lesson is made without the answer being played: at each
//! tick with chance P one fighter's answer to the turn the other really plays is worked out
//! and saved as its lesson, and the game goes on with the turns the plain search chose. The
//! games and their results are then plain self-play; only what the moves are taught changes.
//!
//! --wide makes the search look wider at the root than anywhere else, so self-play tries turns
//! the network thinks little of. With --earned 1 what is saved for the trainer is the visits
//! each turn earned (`Search::root_earned`), without the ones that width alone gave it; the
//! turns played are still picked from the plain visits.
//!
//! With --lead N (the network-led search only) the search may lead its shots, and expects the
//! other fighter to, from N lines through the root on (`Search::lead`).
//!
//! With --net the search is `pvsearch` led by that network. Without it the search is the
//! flagship's (`mcts`, the live playbook's settings): the first generation learns from the
//! search we already have, the following ones from their own play.

use std::io::Write;
use victris_engine::mcts;
use victris_engine::azdata::{shares, shares_earned, GameLog, TickLog};
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
    let net_path: String = arg(&args, "--net", String::new());
    let from_path: String = arg(&args, "--from", String::new());
    let from_share: f64 = arg(&args, "--from-share", 0.5);
    let from_tick: usize = arg(&args, "--from-tick", 0);
    let from_level: i32 = arg(&args, "--from-level", 1000);
    let read: f64 = arg(&args, "--read", 0.0);
    let read_lines: u32 = arg(&args, "--read-lines", iters / 2);
    let read_teach: u32 = arg(&args, "--read-teach", 0);
    let teach_answer: f64 = arg(&args, "--teach-answer", 0.0);
    let from: Vec<GameLog> = if from_path.is_empty() { Vec::new() } else { GameLog::read_all(&std::fs::read(&from_path).expect("cannot read the --from games")) };
    let net = if net_path.is_empty() { None } else { Some(PvNet::load(std::path::Path::new(&net_path)).expect("cannot read the network")) };
    let e = Engine::shipped(&map);
    let flag = mcts::Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior: 0.228, ..Default::default() };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = std::time::Instant::now();
    let parts: Vec<(Vec<u8>, [u64; 11])> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut bytes: Vec<u8> = Vec::new();
                    // games, positions, ticks, fighter 0 wins, fighter 1 wins, search lines, games from a saved position
                    let mut st = [0u64; 11];
                    loop {
                        let g = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if g >= games {
                            break;
                        }
                        let mut rng = Rng::new(seed.wrapping_mul(1_000_003).wrapping_add(g as u64));
                        let mut s = e.new_match((seed as i64).wrapping_mul(7919).wrapping_add(g as i64));
                        let match_seed = (seed as i64).wrapping_mul(7919).wrapping_add(g as i64);
                        let mut log = GameLog { seed: match_seed, z: 0, ticks: Vec::new() };
                        if !from.is_empty() && rng.unit() < from_share {
                            for _ in 0..200 {
                                let real = &from[rng.below(from.len())];
                                if real.ticks.len() < from_tick + 10 {
                                    continue;
                                }
                                let at = from_tick + rng.below(real.ticks.len() - 5 - from_tick);
                                let mut t = e.new_match(real.seed);
                                for k in &real.ticks[..at] {
                                    if t.result.is_some() {
                                        break;
                                    }
                                    e.step(&mut t, &k.turns);
                                }
                                if t.result.is_some() || t.tick < at as i32 || (t.fighters[0].hp - t.fighters[1].hp).abs() > from_level {
                                    continue;
                                }
                                s = t;
                                log = GameLog { seed: real.seed, z: 0, ticks: real.ticks[..at].iter().map(|k| TickLog { turns: k.turns, q: None, pol: None }).collect() };
                                st[6] += 1;
                                break;
                            }
                        }
                        let mut searched = 0u64;
                        let mut pv = net.as_ref().map(|n| pvsearch::Search::new(&e, n, PvConfig { even_root: wide, lead_at: lead, ..Default::default() }));
                        while s.result.is_none() {
                            let free = |x: usize| s.fighters[x].alive && s.fighters[x].state == FState::Idle;
                            if !free(0) && !free(1) {
                                e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
                                log.ticks.push(TickLog { turns: [RawTurn::IDLE, RawTurn::IDLE], q: None, pol: None });
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
                            let mut pair = match &drawn {
                                Some(d) => [pick(&d[0], 1.0, &mut rng), pick(&d[1], 1.0, &mut rng)],
                                None => [pick(&visits[0], w, &mut rng), pick(&visits[1], w, &mut rng)],
                            };
                            if read > 0.0 && visits[0].len() > 1 && visits[1].len() > 1 && rng.unit() < read {
                                // one fighter is told the other's turn and answers it
                                let reader = rng.below(2);
                                if let Some(got) = pv.as_mut().and_then(|p| p.answer_lines(&s, 1 - reader, &pair[1 - reader], read_lines)) {
                                    let mut best = 0usize;
                                    for i in 1..got.len() {
                                        if got[i].1 > got[best].1 || (got[i].1 == got[best].1 && got[i].2 > got[best].2) {
                                            best = i;
                                        }
                                    }
                                    let t = got[best].0;
                                    st[7] += 1;
                                    st[8] += (t != pair[reader]) as u64;
                                    pair[reader] = t;
                                    if read_teach > 0 && got.len() > 1 {
                                        let answer: Vec<(RawTurn, u32)> = got.iter().map(|g| (g.0, g.1.saturating_sub(1))).collect();
                                        if answer.iter().any(|a| a.1 > 0) {
                                            let mut both = pol.take().unwrap_or_else(|| [shares(&visits[0]), shares(&visits[1])]);
                                            both[reader] = shares(&answer);
                                            pol = Some(both);
                                        }
                                    }
                                }
                            } else if teach_answer > 0.0 && visits[0].len() > 1 && visits[1].len() > 1 && rng.unit() < teach_answer {
                                // one fighter's answer to the turn the other plays is its lesson; it is not played
                                let reader = rng.below(2);
                                if let Some(got) = pv.as_mut().and_then(|p| p.answer_lines(&s, 1 - reader, &pair[1 - reader], read_lines)) {
                                    let answer: Vec<(RawTurn, u32)> = got.iter().map(|g| (g.0, g.1.saturating_sub(1))).collect();
                                    if let Some(top) = answer.iter().max_by_key(|a| a.1).filter(|a| a.1 > 0) {
                                        st[9] += 1;
                                        st[10] += (top.0 != pair[reader]) as u64;
                                        let mut both = pol.take().unwrap_or_else(|| [shares(&visits[0]), shares(&visits[1])]);
                                        both[reader] = shares(&answer);
                                        pol = Some(both);
                                    }
                                }
                            }
                            log.ticks.push(TickLog { turns: pair, q, pol: Some(pol.unwrap_or_else(|| [shares(&visits[0]), shares(&visits[1])])) });
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
    let mut st = [0u64; 11];
    for (bytes, s) in &parts {
        file.write_all(bytes).unwrap();
        for i in 0..11 {
            st[i] += s[i];
        }
    }
    file.flush().unwrap();
    let secs = started.elapsed().as_secs_f64();
    println!(
        "azgen {}: {} games ({} from a saved position; a fighter was told the other's turn at {} ticks and changed its own at {}; taught its answer unplayed at {} ticks, not the turn it played at {}), {} positions, {:.0} ticks a game, fighter 0 won {} / fighter 1 won {} / drawn {}, {:.1} s ({:.1} games a minute, {:.0} lines a second a thread)",
        if net.is_some() { "network" } else { "flagship" },
        st[0],
        st[6],
        st[7],
        st[8],
        st[9],
        st[10],
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
