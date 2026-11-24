//! pvedge: what a deeper search's choice is worth over a shallower one's and over the network's
//! own first choice, counted in the deep search's own results.
//!
//!   pvedge --net NET.json --az GAMES.az [--map arena-21] [--at 800,3200,12800,60000]
//!          [--max 400] [--seed 1] [--nodes 150000] [--lead 256] [--threads 4] [--kinds 0.1]
//!
//! Replays the games of GAMES.az, draws --max of their searched positions and searches each
//! one afresh, the way a live bot's search is set up, to the last number of lines in --at.
//! On the way it notes each fighter's most visited turn at every earlier number in --at, and
//! the turn the network gave the largest share before any line was searched. At the end every
//! one of those turns has a mean result in the finished search. The table says, for the
//! network's first choice and for each earlier stop: how often it is not the finished search's
//! turn, how much lower its mean result is then (results run from -1 lost to 1 won), how
//! often it is lower by more than 0.02, 0.05, 0.1 and 0.2, and the loss averaged over every
//! decision. That last figure is what stopping there, or playing the network's first choice
//! unsearched, costs a decision as the deep search counts it.
//!
//! --kinds X adds a table of the decisions where the network's first choice is lower by more
//! than X: the kind of turn the network chose and the kind the finished search plays.
//!
//! A turn with few visits has a rough mean result, and a search gives few visits to a turn
//! that started badly, so the cost of a turn the search dropped early is overstated a little.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use victris_engine::azdata::GameLog;
use victris_engine::pvnet::{class_of, PvNet, ATTACKS};
use victris_engine::pvsearch::{self, PvConfig, Rng};
use victris_engine::state::State;
use victris_engine::Engine;

/// One fighter's decision at one position: per stop (the network's first choice is stop 0)
/// whether the turn differs from the finished search's and by how much its result is lower;
/// then the share of the finished search's visits on the network's first choice.
struct Row {
    lower: Vec<Option<f32>>,
    net_visits: f32,
    value_moved: f32,
    /// the kind of turn the network chose first and the kind the finished search plays
    kinds: (usize, usize),
    /// the position: opponent in sight, a shot in flight, fighters within 2 tiles, either
    /// fighter powered, the chooser behind on hp; and the distance between the fighters
    sit: [bool; 5],
    dist: i32,
}

/// stay, step or dash, times the attack (none or which): 12 kinds.
fn kind_of(class: usize) -> usize {
    let (m, a) = (class / ATTACKS, class % ATTACKS);
    (if m == 0 { 0 } else if m <= 8 { 1 } else { 2 }) * 4 + a
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: u64| opt(name).and_then(|s| s.parse().ok()).unwrap_or(d);
    let map = opt("--map").unwrap_or_else(|| "arena-21".to_string());
    let at: Vec<u32> = opt("--at").unwrap_or_else(|| "800,3200,12800,60000".to_string()).split(',').filter_map(|s| s.trim().parse().ok()).collect();
    assert!(!at.is_empty() && at.windows(2).all(|w| w[0] < w[1]), "--at wants rising numbers of lines");
    let kinds_over: Option<f32> = opt("--kinds").and_then(|s| s.parse().ok());
    let (max, seed, nodes, lead, threads) = (num("--max", 400) as usize, num("--seed", 1), num("--nodes", 150_000) as usize, num("--lead", 256) as u32, num("--threads", 4) as usize);
    let net = PvNet::load(std::path::Path::new(&opt("--net").expect("--net NET.json"))).expect("cannot read the network");
    let e = Engine::shipped(&map);
    let games = GameLog::read_all(&std::fs::read(opt("--az").expect("--az GAMES.az")).expect("cannot read the games"));
    let mut all: Vec<State> = Vec::new();
    for g in &games {
        let mut s = e.new_match(g.seed);
        for t in &g.ticks {
            if s.result.is_some() {
                break;
            }
            if t.pol.is_some() {
                all.push(s);
            }
            e.step(&mut s, &t.turns);
        }
    }
    let mut rng = Rng::new(seed);
    let mut drawn: Vec<State> = Vec::new();
    while drawn.len() < max && !all.is_empty() {
        drawn.push(all.swap_remove(rng.below(all.len())));
    }
    let (next, rows) = (AtomicUsize::new(0), Mutex::new(Vec::<Row>::new()));
    let started = std::time::Instant::now();
    std::thread::scope(|sc| {
        for _ in 0..threads.max(1) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(s) = drawn.get(i) else { break };
                let mut search = pvsearch::Search::new(&e, &net, PvConfig { lead_at: lead, max_nodes: nodes, ..Default::default() });
                let mut done = 0u32;
                // per stop, each fighter's most visited turn (packed)
                let mut firsts: Vec<[Option<u32>; 2]> = Vec::new();
                for &stop in &at {
                    while done < stop {
                        let chunk = (stop - done).min(500);
                        search.iterate(s, chunk);
                        done += chunk;
                    }
                    let acts = search.root_acts();
                    firsts.push([0, 1].map(|x| acts[x].iter().max_by_key(|a| a.2).and_then(|a| a.0.pack())));
                }
                let acts = search.root_acts();
                let moved = (search.value() - search.net_value()).abs();
                let mut mine = Vec::new();
                for x in 0..2 {
                    if acts[x].len() < 2 {
                        continue;
                    }
                    let total: u32 = acts[x].iter().map(|a| a.2).sum();
                    let result = |code: Option<u32>| acts[x].iter().find(|a| a.0.pack() == code).map(|a| (a.3, a.2));
                    let Some(end) = firsts.last().and_then(|f| result(f[x])) else { continue };
                    let net_first = acts[x].iter().max_by(|a, b| a.1.total_cmp(&b.1)).and_then(|a| a.0.pack());
                    let mut lower = Vec::new();
                    for code in std::iter::once(net_first).chain(firsts.iter().map(|f| f[x])) {
                        lower.push(if code == firsts.last().unwrap()[x] { None } else { result(code).map(|r| end.0 - r.0) });
                    }
                    let kind = |code: Option<u32>| acts[x].iter().find(|a| a.0.pack() == code).map(|a| kind_of(class_of(&a.0))).unwrap_or(0);
                    let kinds = (kind(net_first), kind(firsts.last().unwrap()[x]));
                    let (me, op) = (&s.fighters[x], &s.fighters[1 - x]);
                    let dist = victris_engine::geom::chebyshev(me.pos, op.pos);
                    let sit = [
                        victris_engine::observe::can_see(&e.rules, &e.map, me.pos, op.pos),
                        s.n_projectiles > 0,
                        dist <= 2,
                        me.power_until >= s.tick || op.power_until >= s.tick,
                        me.hp < op.hp,
                    ];
                    mine.push(Row { lower, net_visits: result(net_first).map(|r| r.1 as f32 / total.max(1) as f32).unwrap_or(0.0), value_moved: moved, kinds, sit, dist });
                }
                rows.lock().unwrap().extend(mine);
            });
        }
    });
    let rows = rows.into_inner().unwrap();
    let n = rows.len().max(1) as f32;
    println!(
        "pvedge: {} positions, {} decisions, finished search {} lines ({:.0} s); the search's value is {:.4} from the network's own on average; the network's first choice holds {:.1}% of the finished search's visits",
        drawn.len(),
        rows.len(),
        at.last().unwrap(),
        started.elapsed().as_secs_f64(),
        rows.iter().map(|r| r.value_moved).sum::<f32>() / n,
        100.0 * rows.iter().map(|r| r.net_visits).sum::<f32>() / n
    );
    println!("  {:28} {:>9} {:>10} {:>8} {:>8} {:>8} {:>8} {:>12}", "choice", "differs", "lower by", ">0.02", ">0.05", ">0.1", ">0.2", "cost a turn");
    let names: Vec<String> = std::iter::once("the network's first choice".to_string()).chain(at.iter().map(|a| format!("search stopped at {a}"))).collect();
    for (k, name) in names.iter().enumerate().take(names.len() - 1) {
        let d: Vec<f32> = rows.iter().filter_map(|r| r.lower[k]).collect();
        let over = |x: f32| 100.0 * d.iter().filter(|v| **v > x).count() as f32 / n;
        let sum: f32 = d.iter().sum();
        println!("  {:28} {:>8.1}% {:>10.4} {:>7.1}% {:>7.1}% {:>7.1}% {:>7.1}% {:>12.4}", name, 100.0 * d.len() as f32 / n, sum / d.len().max(1) as f32, over(0.02), over(0.05), over(0.1), over(0.2), sum / n);
    }
    if let Some(over) = kinds_over {
        let names: Vec<String> = e.rules.attacks.iter().map(|a| a.name.clone()).collect();
        let label = |k: usize| format!("{}{}", ["stay", "step", "dash"][k / 4], if k % 4 == 0 { String::new() } else { format!(" + {}", names.get(k % 4 - 1).map(String::as_str).unwrap_or("attack")) });
        let group = |name: &str, pick: &dyn Fn(&Row) -> bool| {
            let g: Vec<&Row> = rows.iter().filter(|r| pick(r)).collect();
            let m = g.len().max(1) as f32;
            let share = |i: usize| 100.0 * g.iter().filter(|r| r.sit[i]).count() as f32 / m;
            println!(
                "  {:34} {:5} decisions: opponent in sight {:5.1}%, a shot in flight {:5.1}%, within 2 tiles {:5.1}%, a fighter powered {:5.1}%, chooser behind on hp {:5.1}%, {:.1} tiles apart",
                name,
                g.len(),
                share(0),
                share(1),
                share(2),
                share(3),
                share(4),
                g.iter().map(|r| r.dist as f32).sum::<f32>() / m
            );
        };
        group("every decision", &|_| true);
        group("first choice is the search's", &|r| r.lower[0].is_none());
        group("first choice lower by more than X", &|r| r.lower[0].is_some_and(|v| v > over));
        let mut by = std::collections::HashMap::<(usize, usize), (u32, f32)>::new();
        for r in rows.iter().filter(|r| r.lower[0].is_some_and(|v| v > over)) {
            let c = by.entry(r.kinds).or_insert((0, 0.0));
            c.0 += 1;
            c.1 += r.lower[0].unwrap_or(0.0);
        }
        let mut list: Vec<_> = by.into_iter().collect();
        list.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        let all: u32 = list.iter().map(|r| r.1 .0).sum();
        println!("  the network's first choice lower by more than {over} ({all} decisions): the network's kind, the search's, share, lower by");
        for ((a, b), (c, sum)) in list.iter().take(16) {
            println!("    {:22} {:22} {:5.1}% {:8.3}", label(*a), label(*b), 100.0 * *c as f32 / all.max(1) as f32, sum / *c as f32);
        }
    }
}
