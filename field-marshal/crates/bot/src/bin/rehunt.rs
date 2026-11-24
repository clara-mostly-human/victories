//! rehunt: how fast a playbook finds a hidden opponent again, measured on real games.
//!
//!   rehunt --list FILES.txt --who NAME [--vs NAME*] [--map M] --book A.json [--book B.json ...]
//!          [--max 60] [--min-power 10] [--limit N] [--threads T] [--iters N] [--oracle NAME] [--chaser] [--min-real N]
//!
//! FILES.txt names ladder replays (plain .jsonl), one a line. In each game of fighter X
//! (--who) every spell is taken in which the other fighter goes out of X's sight. While
//! neither sees the other, the other fighter's turns cannot depend on what X does, so its
//! recorded turns are replayed as they were and X is played again from the first hidden tick
//! by each playbook, through the same tracker and `decide` as the live bot (the tracker is
//! fed the real game up to there). The replay of a spell stops when X sees the other again,
//! at the tick the real X saw it again (after that the recorded turns answer the real X), or
//! after --max ticks.
//!
//! Spells are sorted by who held the power-up alone at the first hidden tick (X, the other,
//! neither or both) and by how long the real spell lasted (1-10 ticks, 11 and more; a long
//! real spell is one the real bot failed to end, so its rows flatter any other way of playing).
//! --oracle NAME tells that playbook's learned search where the other really stands: the
//! worth of a perfect guess. For each playbook: how many spells ended with the other in sight before
//! the real game's did, how many were still hidden when the real game's ended, the ticks
//! hidden, and for X powered the power ticks X still had when the other came into sight
//! (nothing if it was still hidden when the replay stopped). The real game's own figures are
//! printed beside them. Nothing here is a result of a game: it measures hunting and hiding
//! only, against the hidden paths real opponents took.

use marshal::policy::{decide, pv_forget, Playbook};
use marshal::tracker::Tracker;
use serde_json::Value;
use victris_engine::events::{visible_events, Event};
use victris_engine::observe::View;
use victris_engine::replay::parse_replay;
use victris_engine::turn::RawTurn;
use victris_engine::State;

const CHASER: &str = "(chaser)";

/// The guesses at the hidden opponent against the truth, over every playbook that decides by
/// guesses, by ticks hidden (1-5, 6-10, 11-20, 21-40, more): decisions, weight on the true tile
/// (millionths), likeliest guess within one tile, tiles it was off (sum), tiles the tracker
/// held possible (sum), the true tile among them.
static GUESS: [[std::sync::atomic::AtomicU64; 6]; 5] = [const { [const { std::sync::atomic::AtomicU64::new(0) }; 6] }; 5];

fn named(want: &Option<String>, name: &str) -> bool {
    match want {
        Some(w) => match w.strip_suffix('*') {
            Some(start) => name.to_lowercase().starts_with(&start.to_lowercase()),
            None => name == w,
        },
        None => true,
    }
}

/// Ticks of the power-up a fighter still has at `tick` (0 without it).
fn power(f: &victris_engine::Fighter, tick: i32) -> i32 {
    if f.power_until >= tick {
        f.power_until - tick + 1
    } else {
        0
    }
}

/// One spell's outcome for one player of X: ticks until the other was in sight (or the
/// replay stopped), whether it was in sight then, X's power ticks left then, the tiles
/// between them then.
#[derive(Clone, Copy, Default)]
struct Out {
    ticks: i32,
    found: bool,
    power_left: i32,
    apart: i32,
}

#[derive(Clone, Default)]
struct Sum {
    spells: u32,
    /// the real game: ticks hidden (capped at --max), found within the cap, power left then
    real: [f64; 3],
    /// by playbook: found before the real game did, still hidden when it did, found at the same
    /// tick, ticks hidden, found within the replay, power left when found, tiles apart at the end
    books: Vec<[f64; 7]>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let list = opt("--list").expect("--list FILES.txt");
    let who = opt("--who");
    let vs = opt("--vs");
    let map = opt("--map");
    let max: i32 = opt("--max").and_then(|s| s.parse().ok()).unwrap_or(60);
    let min_power: i32 = opt("--min-power").and_then(|s| s.parse().ok()).unwrap_or(10);
    let limit: usize = opt("--limit").and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let threads: usize = opt("--threads").and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: Option<u64> = opt("--iters").and_then(|s| s.parse().ok());
    // --trace N: the first N replayed spells of 11 ticks and more with X powered alone, tick by
    // tick for every playbook: where both stand, the walking distance, the turn sent and why
    let trace: usize = opt("--trace").and_then(|s| s.parse().ok()).unwrap_or(0);
    let traced = std::sync::atomic::AtomicUsize::new(0);
    // --min-real N: only spells that lasted at least N ticks in the real game
    let min_real: usize = opt("--min-real").and_then(|s| s.parse().ok()).unwrap_or(0);
    // --oracle NAME[,NAME]: those playbooks' learned search is told where the other really stands
    // (for measuring what a perfect guess would be worth; needs `tree_pv_fog_ticks` to cover the tick)
    let oracle: Vec<String> = opt("--oracle").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default();
    let mut books: Vec<(String, Playbook)> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a == "--book" {
            let path = args[i + 1].clone();
            let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap_or_else(|e| panic!("{path}: {e}"));
            if let Some(d) = v.get("default").cloned() {
                v = d;
            }
            if let Some(n) = iters {
                v["tree_iters"] = Value::from(n);
            }
            let name = std::path::Path::new(&path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(path.clone());
            books.push((name.clone(), Playbook::from_json(&name, &v)));
        }
    }
    // --chaser: one more row, not a bot: X told where the other stands and walking the shortest
    // way to it, a step whenever it may step, no dash and no attack. How soon sight can be had
    // by walking alone.
    if args.iter().any(|a| a == "--chaser") {
        books.push(("(walks straight at it)".to_string(), Playbook { name: CHASER.to_string(), ..Playbook::default() }));
    }
    let files: Vec<String> = std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let taken = std::sync::atomic::AtomicUsize::new(0);
    // sums by class: 0 X powered alone, 1 the other powered alone, 2 neither or both
    let total = std::sync::Mutex::new(vec![Sum { books: vec![[0.0; 7]; books.len()], ..Default::default() }; 6]);
    let games = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(path) = files.get(j) else { break };
                if taken.load(std::sync::atomic::Ordering::Relaxed) >= limit {
                    break;
                }
                let Ok(text) = std::fs::read_to_string(path) else { continue };
                let Ok(r) = parse_replay(&text) else { continue };
                if r.bots.len() < 2 || map.as_ref().map(|m| &r.engine.map.name != m).unwrap_or(false) {
                    continue;
                }
                let xs: Vec<usize> = (0..2).filter(|&x| named(&who, &r.bots[x]) && named(&vs, &r.bots[1 - x])).collect();
                if xs.is_empty() {
                    continue;
                }
                taken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                games.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let e = &r.engine;
                // the real game: the state before every tick and the events of the step into it
                let mut states: Vec<State> = Vec::new();
                let mut evs: Vec<Vec<Event>> = Vec::new();
                let mut s = e.new_match(r.seed);
                let mut events: Vec<Event> = Vec::new();
                for t in r.ticks.iter() {
                    if s.result.is_some() {
                        break;
                    }
                    states.push(s);
                    evs.push(events.clone());
                    e.step_events(&mut s, &t.turns, &mut events);
                }
                let n = states.len();
                let mut local = vec![Sum { books: vec![[0.0; 7]; books.len()], ..Default::default() }; 6];
                for &x in &xs {
                    let seen: Vec<bool> = states.iter().map(|s| View::from_state(e, s, x).enemy.is_some()).collect();
                    let mut t = 1;
                    while t < n {
                        if seen[t] || !seen[t - 1] {
                            t += 1;
                            continue;
                        }
                        // a spell starts at t; the real one ends at t1 (the other in sight again, or the game's end)
                        let t1 = (t..n).find(|&k| seen[k]).unwrap_or(n);
                        let (me, op) = (states[t].fighters[x], states[t].fighters[1 - x]);
                        let (mine, theirs) = (power(&me, states[t].tick), power(&op, states[t].tick));
                        let class = if mine >= min_power && theirs <= 0 {
                            0
                        } else if theirs >= min_power && mine <= 0 {
                            1
                        } else {
                            2
                        };
                        if t1 - t < min_real {
                            t = t1.max(t + 1);
                            continue;
                        }
                        let class = 2 * class + (t1 - t > 10) as usize;
                        let stop = t1.min(t + max as usize).min(n);
                        let real_found = t1 < n && t1 <= t + max as usize;
                        let real = Out {
                            ticks: (stop - t) as i32,
                            found: real_found,
                            power_left: if real_found { power(&states[t1].fighters[x], states[t1].tick) } else { 0 },
                            apart: 0,
                        };
                        let sum = &mut local[class];
                        sum.spells += 1;
                        sum.real[0] += real.ticks as f64;
                        sum.real[1] += real.found as u32 as f64;
                        sum.real[2] += real.power_left as f64;
                        let tracing = class == 1 && traced.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < trace;
                        if tracing {
                            let hp = |k: usize| (states[k.min(n - 1)].fighters[x].hp, states[k.min(n - 1)].fighters[1 - x].hp);
                            println!(
                                "spell: {} seed {} X = fighter {} ({}) against {}, tick {} to {} in the real game; hp X-other at the start {:?}, when seen again {:?}, 50 ticks on {:?}, last tick {:?} of {}; winner {:?}",
                                e.map.name, r.seed, x + 1, r.bots[x], r.bots[1 - x], t, t1, hp(t), hp(t1), hp(t1 + 50), hp(n - 1), n, r.result.as_ref().and_then(|w| w.winner)
                            );
                            // the real game through the spell, every fourth tick
                            for k in (t..t1.min(n)).step_by(4) {
                                let (me, op) = (states[k].fighters[x], states[k].fighters[1 - x]);
                                println!("  real         tick {:>3} X {:?} hp {} | other {:?} hp {} | walk {}", states[k].tick, me.pos, me.hp, op.pos, op.hp, victris_engine::strategy::path(e, op.pos, me.pos));
                            }
                        }
                        for (b, (name, book)) in books.iter().enumerate() {
                            let o = replay_spell(e, r.seed, x, &states, &evs, &r.ticks.iter().map(|k| k.turns).collect::<Vec<_>>(), t, stop, book, oracle.iter().any(|o| o == name), if tracing { Some(name.as_str()) } else { None });
                            let row = &mut sum.books[b];
                            if o.found && o.ticks < real.ticks {
                                row[0] += 1.0;
                            } else if !o.found && real.found {
                                row[1] += 1.0;
                            } else if o.found && real.found {
                                row[2] += 1.0;
                            }
                            row[3] += o.ticks as f64;
                            row[4] += o.found as u32 as f64;
                            row[5] += o.power_left as f64;
                            row[6] += o.apart as f64;
                        }
                        t = t1.max(t + 1);
                    }
                }
                let mut tot = total.lock().unwrap();
                for c in 0..6 {
                    tot[c].spells += local[c].spells;
                    for k in 0..3 {
                        tot[c].real[k] += local[c].real[k];
                    }
                    for b in 0..books.len() {
                        for k in 0..7 {
                            tot[c].books[b][k] += local[c].books[b][k];
                        }
                    }
                }
            });
        }
    });
    let tot = total.lock().unwrap();
    for (b, label) in ["1-5", "6-10", "11-20", "21-40", "41+"].iter().enumerate() {
        let v: Vec<f64> = GUESS[b].iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed) as f64).collect();
        if v[0] > 0.0 {
            println!(
                "guesses, hidden {label} ticks: {} decisions; weight on the true tile {:.0}%; likeliest guess within one tile {:.0}%, off by {:.1} tiles; the tracker holds {:.0} tiles possible, the true one among them {:.0}%",
                v[0], v[1] / v[0] / 1e4, 100.0 * v[2] / v[0], v[3] / v[0], v[4] / v[0], 100.0 * v[5] / v[0]
            );
        }
    }
    println!("rehunt: {} games of {}; spells replayed up to {max} ticks", games.load(std::sync::atomic::Ordering::Relaxed), who.clone().unwrap_or_else(|| "anyone".into()));
    for c in 0..6 {
        let label = format!("{}, the real spell {} ticks", ["X powered alone when the other goes out of sight", "the other powered alone", "neither or both powered"][c / 2], ["1-10", "11+"][c % 2]);
        let s = &tot[c];
        let n = s.spells.max(1) as f64;
        println!("== {label}: {} spells", s.spells);
        println!("   {:<22} {:>12} {:>12} {:>12} {:>12} {:>12} {:>14} {:>12}", "", "ticks hidden", "found %", "sooner %", "same tick %", "later %", "power left", "apart at end");
        println!("   {:<22} {:>12.1} {:>12.0} {:>12} {:>12} {:>12} {:>14.1} {:>12}", "the real game", s.real[0] / n, 100.0 * s.real[1] / n, "", "", "", s.real[2] / n, "");
        for (b, (name, _)) in books.iter().enumerate() {
            let r = &s.books[b];
            println!("   {:<22} {:>12.1} {:>12.0} {:>12.0} {:>12.0} {:>12.0} {:>14.1} {:>12.1}", name, r[3] / n, 100.0 * r[4] / n, 100.0 * r[0] / n, 100.0 * r[2] / n, 100.0 * r[1] / n, r[5] / n, r[6] / n);
        }
    }
}

/// Plays X again from tick `t` (the first hidden tick) to `stop` with `book`, the other
/// fighter's recorded turns replayed. The tracker is fed the real game up to `t`, and the
/// search is run (its turn not used) on the two ticks before, so what the live bot carries
/// into a hidden tick is there.
#[allow(clippy::too_many_arguments)]
fn replay_spell(e: &victris_engine::Engine, seed: i64, x: usize, states: &[State], evs: &[Vec<Event>], turns: &[[RawTurn; 2]], t: usize, stop: usize, book: &Playbook, oracle: bool, trace: Option<&str>) -> Out {
    pv_forget();
    let mut tracker = Tracker::for_engine(e.clone(), x, seed);
    let mut prev: Option<RawTurn> = None;
    for k in 0..t {
        let view = View::from_state(e, &states[k], x);
        let told: Vec<Value> = visible_events(e, &states[k], x, &evs[k]).iter().map(|ev| ev.to_json(e)).collect();
        tracker.observe(&view, &told, prev);
        if k + 2 >= t && book.name != CHASER {
            let _ = decide(&tracker, &view, book);
        }
        prev = Some(turns[k][x]);
    }
    let mut s = states[t];
    let mut events: Vec<Event> = evs[t].clone();
    let mut k = t;
    loop {
        let view = View::from_state(e, &s, x);
        let power_left = power(&s.fighters[x], s.tick);
        let apart = victris_engine::geom::chebyshev(s.fighters[x].pos, s.fighters[1 - x].pos);
        if view.enemy.is_some() {
            return Out { ticks: (k - t) as i32, found: true, power_left, apart };
        }
        if k >= stop || s.result.is_some() {
            return Out { ticks: (k - t) as i32, found: false, power_left: 0, apart };
        }
        let told: Vec<Value> = visible_events(e, &s, x, &events).iter().map(|ev| ev.to_json(e)).collect();
        tracker.observe(&view, &told, prev);
        if book.name == CHASER {
            let to = s.fighters[1 - x].pos;
            let turn = view.moves.iter().min_by_key(|(_, at)| victris_engine::strategy::path(e, to, *at)).map(|(d, _)| RawTurn::step(*d)).unwrap_or(RawTurn::IDLE);
            let mut both = turns[k];
            both[x] = turn;
            prev = Some(turn);
            e.step_events(&mut s, &both, &mut events);
            k += 1;
            continue;
        }
        marshal::policy::PV_ORACLE.with(|o| *o.borrow_mut() = if oracle { Some(s) } else { None });
        marshal::policy::PV_TRUTH.with(|o| *o.borrow_mut() = Some(s));
        marshal::policy::PV_GUESS_LAST.with(|l| l.set(None));
        let d = decide(&tracker, &view, book);
        marshal::policy::PV_ORACLE.with(|o| *o.borrow_mut() = None);
        marshal::policy::PV_TRUTH.with(|o| *o.borrow_mut() = None);
        if let Some(g) = marshal::policy::PV_GUESS_LAST.with(|l| l.get()) {
            // the bot's picture of the hidden opponent against where it really is, by ticks hidden
            let b = match k - t + 1 {
                ..=5 => 0,
                6..=10 => 1,
                11..=20 => 2,
                21..=40 => 3,
                _ => 4,
            };
            GUESS[b][0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            GUESS[b][1].fetch_add((g.on_truth * 1e6) as u64, std::sync::atomic::Ordering::Relaxed);
            GUESS[b][2].fetch_add((g.off_by <= 1) as u64, std::sync::atomic::Ordering::Relaxed);
            GUESS[b][3].fetch_add(g.off_by as u64, std::sync::atomic::Ordering::Relaxed);
            GUESS[b][4].fetch_add(g.possible as u64, std::sync::atomic::Ordering::Relaxed);
            GUESS[b][5].fetch_add(g.truth_possible as u64, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(name) = trace {
            let (me, op) = (s.fighters[x], s.fighters[1 - x]);
            println!(
                "  {name:<12} tick {:>3} X {:?} hp {} power {} | other {:?} hp {} | walk {} | {} {}",
                s.tick, me.pos, me.hp, power_left, op.pos, op.hp, victris_engine::strategy::path(e, op.pos, me.pos), d.note, marshal::session::turn_json(s.tick, &d.turn, e)
            );
            // what the search made of it: the root's value from X's side and X's most searched turns (share of lines, mean result)
            marshal::policy::LAST_ROOT.with(|l| {
                if let Some(r) = l.borrow().as_ref() {
                    if (r.root.tick - s.tick).abs() <= 1 {
                        let mut mine: Vec<(RawTurn, u32, f64)> = r.acts[x].clone();
                        mine.sort_by(|a, b| b.1.cmp(&a.1));
                        let total: u32 = mine.iter().map(|a| a.1).sum();
                        let top: Vec<String> = mine.iter().take(4).map(|a| format!("{} {:.0}% {:+.3}", marshal::session::turn_json(s.tick, &a.0, e), 100.0 * a.1 as f64 / total.max(1) as f64, a.2)).collect();
                        println!("               value {:+.3} | {}", if x == 0 { r.value } else { -r.value }, top.join(" | "));
                    }
                }
            });
        }
        if book.tree_pv_ponder > 0 && d.note == "pv" {
            marshal::policy::pv_ponder(&tracker, book, book.tree_pv_ponder, None);
        }
        let mut both = turns[k];
        both[x] = d.turn;
        prev = Some(d.turn);
        e.step_events(&mut s, &both, &mut events);
        k += 1;
    }
}
