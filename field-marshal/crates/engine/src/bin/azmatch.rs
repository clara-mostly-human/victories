//! azmatch: plays two searches against each other on the exact engine, everything in sight.
//!
//!   azmatch --a SPEC --b SPEC [--games 100] [--seed 1] [--threads 4] [--map columns-21] [--open 10]
//!           [--book GAMES.az] [--book-from 20] [--book-to 220] [--soft 0]
//!           [--read-a 0] [--read-b 0] [--read-lines N] [--read-usual 0]
//!           [--draw-a 0] [--draw-b 0] [--draw-power 1]
//!
//! SPEC is `flag:N` (the flagship's search, the live playbook's settings, N lines a tick),
//! `pv:NET.json:N` (the network-led search), `pvw:NET.json:N[:SHARE[:TRUST]]` (the same, its
//! turn chosen by `Search::weigh` with SHARE of the N lines, a fifth unless given, spent
//! weighing, and TRUST, 1 unless given, in the turns expected of the opponent),
//! `pvd:NET.json:N:K` (the network-led search with K times the lines at a standoff: what a
//! memory of that one situation, searched deep beforehand, could be worth),
//! `pvt:NET.json:N:K` (the network-led search on K threads, `Search::iterate_threads`;
//! against `pv` at the same N it shows what a line from several threads is worth),
//! `pvl:NET.json:N[:AT]` (the network-led search with lead shots, `Search::lead`, from AT
//! lines on, 256 unless given), `pvn:NET.json:N:NODES` (the network-led search with a tree
//! of at most NODES positions, `PvConfig::max_nodes`), `pve:NET.json:N:EVEN` (lead shots as
//! `pvl`, with the share EVEN of every position's turn weights spread evenly, `PvConfig::even`:
//! 1 takes the network's turn shares out of the search, which shows what they are worth to it
//! at N lines), `pvm:NET.json:N[:SHARE[:EPS]]` (lead
//! shots as `pvl`; SHARE of the N lines, a quarter unless given, goes to a last step: the
//! turns the search took seriously and rates within EPS, 0.02 unless given, of its most
//! visited one are set against the other fighter's answers in a small table, the table is
//! solved and the turn is drawn by the solved shares, `Search::mixed`; with nothing rated
//! the same the lines go to the search itself), or
//! `flag`/`pv` with `Nms` in place of N for a time limit a tick. Each fighter runs its own search every tick it can act, fresh, and plays its
//! most visited turn. Both searches are repeatable, so without help every game would be one of
//! two: each pair of games starts with --open ticks of random steps (no attacks, no dashes),
//! the same steps for both games of the pair, and the two players swap sides between them.
//! Those openings are not enough: nothing has happened yet, so many games settle into the same
//! few games anyway. With --book each pair instead starts from a position out of a saved game
//! (azgen's file): a random tick from --book-from to --book-to (20 to 220 unless given) with both
//! alive and the hp within 20. A later range judges play in the small last zone.
//! --soft K tests play from the spawn instead: before tick K each player draws its turn in
//! proportion to its own search's visits, so every game is different and each player still
//! plays its own opening (use with --open 0).
//! --read-b P: at each tick with chance P player b (a network-led one) is told the turn a has
//! just chosen and plays its best answer to it (`Search::answer`, --read-lines more lines,
//! half of b's own unless given); --read-a the other way round. No bot gets that in a game.
//! It measures what a's play gives away to an opponent that guesses its turn: run the same
//! b against two a's and compare.
//! --draw-a P (--draw-b): at each tick with chance P that player plays a turn drawn in
//! proportion to its search's visits (each raised to --draw-power) in place of its most
//! visited one: a mix where the plain player has a habit. With --read-usual 1 the reader is
//! told the other player's most visited turn whatever that player then plays: an opponent
//! that knows our habit exactly but cannot see a draw. Together they measure what a mix
//! costs against a plain opponent and what it saves against one that knows the habit.

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
    /// lead shots from this many lines through the root on (`PvConfig::lead_at`); 0: none
    lead: u32,
    /// the most positions in the tree (`PvConfig::max_nodes`); 0: the default
    nodes: usize,
    /// share of the lines spent on the table of the turns the search rates the same and
    /// the other fighter's answers to them (`Search::mixed`), the turn drawn by its solved
    /// shares; 0: off
    mix: f64,
    /// turns within this of the most visited turn's result count as rated the same
    mix_eps: f32,
    /// the share of every position's turn weights spread evenly (`PvConfig::even`, at the
    /// root too); None: the search's own setting
    even: Option<f32>,
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
        ["flag", l] => Player { net: None, limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: 1, lead: 0, nodes: 0, mix: 0.0, mix_eps: 0.0, even: None },
        ["pv", path, l] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: 1, lead: 0, nodes: 0, mix: 0.0, mix_eps: 0.0, even: None },
        ["pvd", path, l, k] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: k.parse().expect("bad multiple"), search_threads: 1, lead: 0, nodes: 0, mix: 0.0, mix_eps: 0.0, even: None },
        ["pvt", path, l, k] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: k.parse().expect("bad thread count"), lead: 0, nodes: 0, mix: 0.0, mix_eps: 0.0, even: None },
        ["pvw", path, l, rest @ ..] if rest.len() <= 2 => Player {
            net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")),
            limit: limit(l),
            weigh: rest.first().map(|w| w.parse().expect("bad share")).unwrap_or(0.2),
            trust: rest.get(1).map(|w| w.parse().expect("bad trust")).unwrap_or(1.0),
            deep: 1,
            search_threads: 1,
            lead: 0,
            nodes: 0,
            mix: 0.0,
            mix_eps: 0.0,
            even: None,
        },
        ["pvl", path, l, rest @ ..] if rest.len() <= 1 => Player {
            net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")),
            limit: limit(l),
            weigh: 0.0,
            trust: 1.0,
            deep: 1,
            search_threads: 1,
            lead: rest.first().map(|w| w.parse().expect("bad line count")).unwrap_or(256),
            nodes: 0,
            mix: 0.0,
            mix_eps: 0.0,
            even: None,
        },
        ["pvm", path, l, rest @ ..] if rest.len() <= 2 => Player {
            net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")),
            limit: limit(l),
            weigh: 0.0,
            trust: 1.0,
            deep: 1,
            search_threads: 1,
            lead: 256,
            nodes: 0,
            mix: rest.first().map(|w| w.parse().expect("bad share")).unwrap_or(0.25),
            mix_eps: rest.get(1).map(|w| w.parse().expect("bad margin")).unwrap_or(0.02),
            even: None,
        },
        ["pve", path, l, ev] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: 1, lead: 256, nodes: 0, mix: 0.0, mix_eps: 0.0, even: Some(ev.parse().expect("bad share")) },
        ["pvn", path, l, n] => Player { net: Some(PvNet::load(std::path::Path::new(path)).expect("cannot read the network")), limit: limit(l), weigh: 0.0, trust: 1.0, deep: 1, search_threads: 1, lead: 0, nodes: n.parse().expect("bad position count"), mix: 0.0, mix_eps: 0.0, even: None },
        _ => panic!("bad player: {spec}"),
    }
}

/// The player's turn for fighter `x` at `s`, the lines it searched, how deep the average line
/// went, its turns with their visits, and for a `pvm` player whether a table was solved this
/// tick and whether the turn drawn (by `draw`, a number from 0 to 1) is not the most visited.
fn think(e: &Engine, p: &Player, flag: mcts::Config, s: &State, x: usize, told: Option<(&RawTurn, u32)>, draw: f64) -> (RawTurn, u64, f64, Vec<(RawTurn, u32)>, [bool; 2]) {
    let mut mixed = [false; 2];
    let started = std::time::Instant::now();
    let mut lines = 0u64;
    match &p.net {
        Some(net) => {
            let mut cfg = PvConfig { lead_at: p.lead, ..Default::default() };
            if p.nodes > 0 {
                cfg.max_nodes = p.nodes;
            }
            if let Some(ev) = p.even {
                cfg.even = ev;
                cfg.even_root = ev;
            }
            let mut search = pvsearch::Search::new(e, net, cfg);
            let mut held = 0u32;
            match p.limit {
                Limit::Lines(n) => {
                    let n = if p.deep > 1 && standoff(e, s) { n * p.deep } else { n };
                    held = (n as f64 * p.weigh.max(p.mix)).round() as u32;
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
            let mut turn = if p.weigh > 0.0 { search.weigh(s, x, held.max(1), p.trust)[0].turn } else { search.best(x) };
            if p.mix > 0.0 && told.is_none() {
                if search.rated_same(x, p.mix_eps).len() < 2 {
                    // nothing to compare: the lines held back go to the search itself
                    search.iterate(s, held);
                    turn = search.best(x);
                } else {
                    let (table, _) = search.mixed(s, x, held.max(1), p.mix_eps);
                    mixed[0] = table.len() > 1;
                    let mut at = draw as f32;
                    for t in table.iter() {
                        if at < t.share {
                            mixed[1] = t.turn != turn;
                            turn = t.turn;
                            break;
                        }
                        at -= t.share;
                    }
                }
            }
            if let Some((theirs, more)) = told {
                // told the other fighter's turn: the best answer to it
                let more = if more > 0 { more } else { (lines / 2).max(1) as u32 };
                if let Some(t) = search.answer(s, 1 - x, theirs, more) {
                    turn = t;
                }
            }
            let depth: u64 = search.depth_hist.iter().enumerate().map(|(d, n)| d as u64 * n).sum();
            (turn, lines, depth as f64 / lines.max(1) as f64, visits, mixed)
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
            (r.acts[x].iter().max_by_key(|a| a.1).map(|a| a.0).unwrap_or(RawTurn::IDLE), lines, 0.0, visits, mixed)
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
    // the ticks a book position may come from
    let book_from: i32 = arg(&args, "--book-from", 20);
    let book_to: i32 = arg(&args, "--book-to", 220);
    let read: [f64; 2] = [arg(&args, "--read-a", 0.0), arg(&args, "--read-b", 0.0)];
    let read_lines: u32 = arg(&args, "--read-lines", 0);
    let read_usual: u32 = arg(&args, "--read-usual", 0);
    let draw: [f64; 2] = [arg(&args, "--draw-a", 0.0), arg(&args, "--draw-b", 0.0)];
    let draw_power: f64 = arg(&args, "--draw-power", 1.0);
    let book: Vec<GameLog> = if book_path.is_empty() { Vec::new() } else { GameLog::read_all(&std::fs::read(&book_path).expect("cannot read the book")) };
    let players = [parse(&sa), parse(&sb)];
    let e = Engine::shipped(&map);
    let burst = e.rules.attacks.iter().position(|a| matches!(a.kind, victris_engine::rules::AttackKind::Burst { .. }));
    let flag = mcts::Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior: 0.228, ..Default::default() };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let started = std::time::Instant::now();
    // per thread: a wins, b wins, draws, a's hp at the end, b's hp at the end, ticks, then per player lines, thinks, depth sum
    let parts: Vec<[f64; 20]> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                sc.spawn(|| {
                    let mut st = [0.0f64; 20];
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
                                        t.tick >= book_from && t.tick <= book_to && t.fighters[0].alive && t.fighters[1].alive && (t.fighters[0].hp - t.fighters[1].hp).abs() <= 20
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
                            // each fighter's most visited turn this tick (what a reader is told with --read-usual)
                            let mut usual = [RawTurn::IDLE, RawTurn::IDLE];
                            // the player that is told the other's turn this tick (0: a, 1: b), if any; it thinks second
                            let can = |x: usize| s.fighters[x].alive && s.fighters[x].state == FState::Idle;
                            let reader: Option<usize> = if (read[0] > 0.0 || read[1] > 0.0) && s.tick >= open && can(0) && can(1) {
                                let (ra, rb) = (own.unit() < read[0], own.unit() < read[1]);
                                match (ra, rb) {
                                    (true, false) => Some(0),
                                    (false, true) => Some(1),
                                    (true, true) => Some(own.below(2)),
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            let order = match reader {
                                Some(who) if (if who == 0 { side } else { 1 - side }) == 0 => [1, 0],
                                _ => [0, 1],
                            };
                            for x in order {
                                if s.tick < open {
                                    victris_engine::pvnet::turns(&e, &s, x, &mut buf);
                                    buf.retain(|t| t.attack == victris_engine::turn::AttackIn::None && t.dash == victris_engine::turn::DirIn::None);
                                    pair[x] = buf[rng.below(buf.len())];
                                } else if s.fighters[x].alive && s.fighters[x].state == FState::Idle {
                                    let who = if x == side { 0 } else { 1 };
                                    let told = if reader == Some(who) { Some((if read_usual > 0 { usual[1 - x] } else { pair[1 - x] }, read_lines)) } else { None };
                                    // a mixing player's draw this tick (none taken for the others, so their games are as they were)
                                    let drawn = if players[who].mix > 0.0 { own.unit() } else { 0.0 };
                                    let (t, lines, depth, visits, mixed) = think(&e, &players[who], flag, &s, x, told.as_ref().map(|(t, n)| (t, *n)), drawn);
                                    st[16 + who] += mixed[0] as u8 as f64;
                                    st[18 + who] += mixed[1] as u8 as f64;
                                    // the habit: the most visited turn (a mixing player's turn `t` may be another)
                                    usual[x] = visits.iter().max_by_key(|v| v.1).map(|v| v.0).unwrap_or(t);
                                    pair[x] = if s.tick < soft {
                                        pick(&visits, 1.0, &mut own)
                                    } else if draw[who] > 0.0 && reader != Some(who) && own.unit() < draw[who] {
                                        // a turn drawn by its visits raised to --draw-power
                                        let weights: Vec<f64> = visits.iter().map(|v| (v.1 as f64).powf(draw_power)).collect();
                                        let total: f64 = weights.iter().sum();
                                        let mut at = own.unit() * total;
                                        let mut drawn = t;
                                        for (v, w) in visits.iter().zip(weights.iter()) {
                                            if at < *w {
                                                drawn = v.0;
                                                break;
                                            }
                                            at -= *w;
                                        }
                                        drawn
                                    } else {
                                        t
                                    };
                                    st[6 + who * 3] += lines as f64;
                                    // how often each player dashes and uses the repulsor
                                    st[12 + who] += (pair[x].dash != victris_engine::turn::DirIn::None) as u8 as f64;
                                    st[14 + who] += matches!(pair[x].attack, victris_engine::turn::AttackIn::Named { attack, .. } if Some(attack as usize) == burst) as u8 as f64;
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
    let mut st = [0.0f64; 20];
    for p in &parts {
        for i in 0..20 {
            st[i] += p[i];
        }
    }
    let n = (st[0] + st[1] + st[2]).max(1.0);
    let score = (st[0] + 0.5 * st[2]) / n;
    let elo = if score > 0.0 && score < 1.0 { -400.0 * (1.0 / score - 1.0).log10() } else if score >= 1.0 { 999.0 } else { -999.0 };
    let margin = 1.96 * (score * (1.0 - score) / n).sqrt();
    println!(
        "azmatch a={sa} b={sb}: a won {} lost {} drew {} of {} ({:.1}% +-{:.1}, {:+.0} Elo); hp at the end a {:.1} b {:.1}; {:.0} ticks a game; lines a tick a {:.0} b {:.0}; average line a {:.1} b {:.1} ticks; dashes a game a {:.1} b {:.1}, repulsors a {:.1} b {:.1}; ticks with a table solved a {:.1}% b {:.1}%, turn other than the most visited a {:.1}% b {:.1}%; {:.0} s",
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
        st[12] / n,
        st[13] / n,
        st[14] / n,
        st[15] / n,
        100.0 * st[16] / st[7].max(1.0),
        100.0 * st[17] / st[10].max(1.0),
        100.0 * st[18] / st[7].max(1.0),
        100.0 * st[19] / st[10].max(1.0),
        started.elapsed().as_secs_f64()
    );
}
