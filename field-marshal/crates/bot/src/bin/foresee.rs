//! foresee: what a playbook's search makes of the fights for a power pad in real games.
//!
//!   foresee --list FILES.txt --who NAME [--vs NAME*] [--map M] [--result W|L] --book A.json [--book B.json ...]
//!           [--before 10] [--after 8] [--until TICK] [--all] [--iters N] [--threads T] [--limit N] [--trace N]
//!
//! FILES.txt names ladder replays (plain .jsonl), one a line. In each game of fighter X
//! (--who) the ticks around every appearing power pad are taken (--before and --after it;
//! --until TICK takes only the pads that appear by that tick, 20 for the first pair; --all
//! takes every tick). On each of those ticks the playbook decides X's turn from the
//! real position, through the same tracker and `decide` as the live bot, and the real game
//! then goes on as it was played: the tracker is always fed the real game, so every decision
//! stands alone and nothing is replayed against turns the other fighter chose for a
//! different game. (On an open map the fighters see each other nearly all game, so the other's
//! recorded turns cannot be replayed for more than the tick they were sent on. For that one
//! tick they can: both turns of a tick are sent without sight of each other.)
//!
//! Three things are read off each decision:
//!  - the other fighter's real turn that tick, by kind: what share of the search's lines had
//!    the other play it (the turns that lead to the position the real turn led to), how often
//!    that share was under 5%, and how often no listed turn leads there at all; and what the
//!    real game did in the 8 ticks after (hp gained by X less hp gained by the other), apart
//!    for the turns the search had under 5% and the rest: what being surprised goes with;
//!  - X's turn from the playbook against the turn X really sent, both played with the other's
//!    real turn of that tick: hp of both, power pads taken, knockbacks taken, tiles to the
//!    nearest power pad that lies or appears within 10 ticks;
//!  - the search's value for X at the first tick of the stretch, when the pad appears and at
//!    the last tick, by who took more power pads in the stretch: whether the search sees the
//!    pad going and what it thinks a pad is worth.
//!
//! --regret N,M adds, on every tick with the other in sight: a fresh search of N lines from
//! the real position, then M lines in which the other's first turn is fixed to the one it
//! really sent and only X's answer varies (`Search::answers`). The best answer's result less
//! the result of the turn the playbook chose, both against the real turn, is what the choice
//! gave up by not knowing that turn (in the search's own value, -1 lost to 1 won). It is
//! summed apart for real turns the search had under 5% of its lines on and for the rest. A
//! change to the search that covers real opponents' turns better shows here as less given up
//! on the first kind without more on the second; an own-bot match cannot show it.
//! Nothing here is a result of a game.

use marshal::policy::{decide, pv_forget, pv_ponder, Playbook, LAST_ROOT};
use marshal::tracker::Tracker;
use serde_json::Value;
use victris_engine::events::{visible_events, Event};
use victris_engine::geom::chebyshev;
use victris_engine::map::PadKind;
use victris_engine::observe::View;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, NO_BUFF};
use victris_engine::turn::RawTurn;
use victris_engine::{Engine, State};

const KINDS: [&str; 6] = ["repulsor", "frost", "shot", "dash, no attack", "step, no attack", "stays, no attack"];
/// How far the real game is followed after a turn of the other's to see what it cost.
const AHEAD: usize = 8;
const TAKERS: [&str; 3] = ["X took more of the pads", "level or none taken", "the other took more"];

fn named(want: &Option<String>, name: &str) -> bool {
    match want {
        Some(w) => match w.strip_suffix('*') {
            Some(start) => name.to_lowercase().starts_with(&start.to_lowercase()),
            None => name == w,
        },
        None => true,
    }
}

#[derive(Clone, Default)]
struct Sum {
    /// the other's real turn by kind: ticks, share of lines on it (sum), share under 5%, not
    /// listed; then what the real game did next (hp gained by X less hp gained by the other
    /// over the following `AHEAD` ticks): its sum and count where the share was under 5%, and
    /// where it was not
    opp: [[f64; 8]; 6],
    /// X's turn: decisions, same as the real one, hp of X (playbook less real), hp of the other,
    /// power pads taken by the playbook's turn, by the real turn, knockbacks taken by the
    /// playbook's turn, by the real turn, ticks with a pad to measure, tiles to it (playbook),
    /// tiles to it (real), decisions made by the learned search
    own: [f64; 12],
    /// by who took more pads in the stretch: stretches, then sum and count of the value for X
    /// at the first tick, at the tick the pad appears, at the last tick
    val: [[f64; 7]; 3],
    /// `--regret`, by kind of the other's real turn: where the search had it under 5%: ticks,
    /// given up (sum), ticks where the best answer was another turn; then the same three
    /// where it had it at 5% or more
    regret: [[f64; 6]; 6],
    /// `--regret`: ticks left out because the playbook's turn was not among the fresh search's turns
    regret_skipped: f64,
}

impl Sum {
    fn add(&mut self, o: &Sum) {
        for k in 0..6 {
            for j in 0..8 {
                self.opp[k][j] += o.opp[k][j];
            }
        }
        for j in 0..12 {
            self.own[j] += o.own[j];
        }
        for k in 0..3 {
            for j in 0..7 {
                self.val[k][j] += o.val[k][j];
            }
        }
        for k in 0..6 {
            for j in 0..6 {
                self.regret[k][j] += o.regret[k][j];
            }
        }
        self.regret_skipped += o.regret_skipped;
    }
}

/// The position without what a shout leaves behind: two turns that differ only in a shout
/// lead to the same fight.
fn phys(mut s: State) -> State {
    for f in s.fighters.iter_mut() {
        f.cd_shout = 0;
    }
    s
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let list = opt("--list").expect("--list FILES.txt");
    let who = opt("--who");
    let vs = opt("--vs");
    let map = opt("--map");
    let result = opt("--result");
    let before: usize = opt("--before").and_then(|s| s.parse().ok()).unwrap_or(10);
    let after: usize = opt("--after").and_then(|s| s.parse().ok()).unwrap_or(8);
    let until: i32 = opt("--until").and_then(|s| s.parse().ok()).unwrap_or(i32::MAX);
    let all = args.iter().any(|a| a == "--all");
    // --regret N,M: see the top of the file
    let regret: Option<(u32, u32)> = opt("--regret").and_then(|s| {
        let mut it = s.split(',').filter_map(|v| v.parse::<u32>().ok());
        Some((it.next()?, it.next()?))
    });
    let limit: usize = opt("--limit").and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let threads: usize = opt("--threads").and_then(|s| s.parse().ok()).unwrap_or(1);
    let iters: Option<u64> = opt("--iters").and_then(|s| s.parse().ok());
    // --trace N: the first N stretches tick by tick for every playbook
    let trace: usize = opt("--trace").and_then(|s| s.parse().ok()).unwrap_or(0);
    let traced = std::sync::atomic::AtomicUsize::new(0);
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
    let files: Vec<String> = std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let taken = std::sync::atomic::AtomicUsize::new(0);
    let total = std::sync::Mutex::new(vec![Sum::default(); books.len()]);
    let counts = std::sync::Mutex::new([0usize; 3]);
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
                let winner = r.result.as_ref().and_then(|w| w.winner);
                let xs: Vec<usize> = (0..2)
                    .filter(|&x| named(&who, &r.bots[x]) && named(&vs, &r.bots[1 - x]))
                    .filter(|&x| match result.as_deref() {
                        Some("W") => winner == Some(x as i64 + 1),
                        Some("L") => winner == Some(2 - x as i64),
                        _ => true,
                    })
                    .collect();
                if xs.is_empty() {
                    continue;
                }
                taken.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let e = &r.engine;
                // the real game: states[k] is the position tick k's turns were sent in, evs[k] the
                // events of the step into it; states[n] is the position after the last turns
                let mut s = e.new_match(r.seed);
                let mut states: Vec<State> = vec![s];
                let mut evs: Vec<Vec<Event>> = vec![Vec::new()];
                let mut events: Vec<Event> = Vec::new();
                let mut turns: Vec<[RawTurn; 2]> = Vec::new();
                for t in r.ticks.iter() {
                    if s.result.is_some() {
                        break;
                    }
                    e.step_events(&mut s, &t.turns, &mut events);
                    turns.push(t.turns);
                    states.push(s);
                    evs.push(events.clone());
                }
                let n = turns.len();
                if n < 2 {
                    continue;
                }
                // the stretches: the ticks around every appearing power pad, joined where they meet
                let appears: Vec<usize> = (1..n).filter(|&k| states[k].tick <= until && evs[k].iter().any(|ev| matches!(ev, Event::PickupSpawn { kind: PadKind::Power, .. }))).collect();
                let mut marked = vec![all; n];
                for &a in &appears {
                    for k in a.saturating_sub(before)..=(a + after).min(n - 1) {
                        marked[k] = true;
                    }
                }
                let mut spans: Vec<(usize, usize)> = Vec::new();
                let mut k = 0;
                while k < n {
                    if marked[k] {
                        let a = k;
                        while k + 1 < n && marked[k + 1] {
                            k += 1;
                        }
                        spans.push((a, k));
                    }
                    k += 1;
                }
                let mut local = vec![Sum::default(); books.len()];
                let mut seen = [0usize; 3];
                for &x in &xs {
                    seen[0] += 1;
                    for &(a, b) in &spans {
                        seen[1] += 1;
                        seen[2] += b - a + 1;
                        let tracing = traced.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < trace;
                        if tracing {
                            println!(
                                "stretch: {} seed {} X = fighter {} ({}) against {}, ticks {} to {}, power pads appear at {:?}; winner {:?}",
                                e.map.name,
                                r.seed,
                                x + 1,
                                r.bots[x],
                                r.bots[1 - x],
                                states[a].tick,
                                states[b].tick,
                                appears.iter().filter(|&&p| p >= a && p <= b).map(|&p| states[p].tick).collect::<Vec<_>>(),
                                winner
                            );
                        }
                        for (i, (name, book)) in books.iter().enumerate() {
                            stretch(e, r.seed, x, &states, &evs, &turns, a, b, &appears, book, regret, &mut local[i], if tracing { Some(name.as_str()) } else { None });
                        }
                    }
                }
                let mut tot = total.lock().unwrap();
                for i in 0..books.len() {
                    tot[i].add(&local[i]);
                }
                let mut c = counts.lock().unwrap();
                for i in 0..3 {
                    c[i] += seen[i];
                }
            });
        }
    });
    let tot = total.lock().unwrap();
    let c = counts.lock().unwrap();
    println!(
        "foresee: {} games of {}, {} stretches, {} ticks{}",
        c[0],
        who.clone().unwrap_or_else(|| "anyone".into()),
        c[1],
        c[2],
        if all { " (every tick)".to_string() } else { format!(" ({before} ticks before a power pad appears to {after} after)") }
    );
    for (i, (name, _)) in books.iter().enumerate() {
        let s = &tot[i];
        println!("== {name}");
        println!("   the other's real turn, in the search's lines:");
        println!("   {:<20} {:>8} {:>16} {:>12} {:>12} {:>24} {:>22}", "", "ticks", "share of lines %", "under 5% %", "not listed %", "hp swing after, under 5%", "hp swing after, else");
        let mut all = [0.0f64; 8];
        for k in 0..6 {
            let v = &s.opp[k];
            let n = v[0].max(1.0);
            println!("   {:<20} {:>8.0} {:>16.1} {:>12.0} {:>12.0} {:>24.2} {:>22.2}", KINDS[k], v[0], 100.0 * v[1] / n, 100.0 * v[2] / n, 100.0 * v[3] / n, v[4] / v[5].max(1.0), v[6] / v[7].max(1.0));
            for j in 0..8 {
                all[j] += v[j];
            }
        }
        let n = all[0].max(1.0);
        println!("   {:<20} {:>8.0} {:>16.1} {:>12.0} {:>12.0} {:>24.2} {:>22.2}", "every kind", all[0], 100.0 * all[1] / n, 100.0 * all[2] / n, 100.0 * all[3] / n, all[4] / all[5].max(1.0), all[6] / all[7].max(1.0));
        let o = &s.own;
        let n = o[0].max(1.0);
        println!("   X's turn from this playbook against the turn X sent, the other's real turn played with both:");
        println!(
            "   {:.0} decisions ({:.0}% by the learned search), same as the real turn {:.0}%; hp of X {:+.1} and of the other {:+.1} a 100 ticks; power pads taken {:.0} v {:.0} real; knockbacks taken {:.0} v {:.0} real; tiles to the pad {:.2} v {:.2} real ({:.0} ticks)",
            o[0],
            100.0 * o[11] / n,
            100.0 * o[1] / n,
            100.0 * o[2] / n,
            100.0 * o[3] / n,
            o[4],
            o[5],
            o[6],
            o[7],
            o[9] / o[8].max(1.0),
            o[10] / o[8].max(1.0),
            o[8]
        );
        if regret.is_some() {
            println!("   what X's turn gave up by not knowing the other's real turn (the search's value, -1 to 1), a 100 ticks of that kind:");
            println!("   {:<20} {:>10} {:>12} {:>16} {:>10} {:>12} {:>16}", "", "under 5%", "given up", "another turn %", "5% or more", "given up", "another turn %");
            let mut all = [0.0f64; 6];
            for k in 0..6 {
                let v = &s.regret[k];
                println!("   {:<20} {:>10.0} {:>12.2} {:>16.0} {:>10.0} {:>12.2} {:>16.0}", KINDS[k], v[0], 100.0 * v[1] / v[0].max(1.0), 100.0 * v[2] / v[0].max(1.0), v[3], 100.0 * v[4] / v[3].max(1.0), 100.0 * v[5] / v[3].max(1.0));
                for j in 0..6 {
                    all[j] += v[j];
                }
            }
            println!("   {:<20} {:>10.0} {:>12.2} {:>16.0} {:>10.0} {:>12.2} {:>16.0}", "every kind", all[0], 100.0 * all[1] / all[0].max(1.0), 100.0 * all[2] / all[0].max(1.0), all[3], 100.0 * all[4] / all[3].max(1.0), 100.0 * all[5] / all[3].max(1.0));
            println!("   given up in all: {:.2} over {:.0} ticks ({:.0} left out: the playbook's turn was not among the fresh search's)", all[1] + all[4], all[0] + all[3], s.regret_skipped);
        }
        println!("   the search's value for X (-1 lost to 1 won) by who took more power pads in the stretch:");
        println!("   {:<26} {:>10} {:>14} {:>18} {:>12}", "", "stretches", "first tick", "the pad appears", "last tick");
        for k in 0..3 {
            let v = &s.val[k];
            println!("   {:<26} {:>10.0} {:>14.3} {:>18.3} {:>12.3}", TAKERS[k], v[0], v[1] / v[2].max(1.0), v[3] / v[4].max(1.0), v[5] / v[6].max(1.0));
        }
    }
}

/// One stretch of the real game (ticks `a` to `b`) decided again by `book` for fighter `x`,
/// one tick at a time from the real positions. The search is also run (its turn not used) on
/// the two ticks before, so the tree the live bot carries into a tick is there.
#[allow(clippy::too_many_arguments)]
fn stretch(e: &Engine, seed: i64, x: usize, states: &[State], evs: &[Vec<Event>], turns: &[[RawTurn; 2]], a: usize, b: usize, appears: &[usize], book: &Playbook, regret: Option<(u32, u32)>, sum: &mut Sum, trace: Option<&str>) {
    let o = 1 - x;
    let net = regret.and_then(|_| victris_engine::pvnet::PvNet::load(std::path::Path::new(&format!("{}/{}.json", book.tree_pv_dir, e.map.name))));
    let index = |want: u8| {
        e.rules.attacks.iter().position(|k| match (&k.kind, want) {
            (AttackKind::Projectile { .. }, 0) => true,
            (AttackKind::Area { .. }, 1) => true,
            (AttackKind::Burst { .. }, 2) => true,
            _ => false,
        })
    };
    let (shot, frost, burst) = (index(0), index(1), index(2));
    let power_pads: Vec<(usize, [i32; 2])> = e.map.pads.iter().enumerate().filter(|(_, d)| d.kind == PadKind::Power).map(|(i, d)| (i, d.pos)).collect();
    let play = |s: &State, mine: RawTurn, theirs: RawTurn| -> (State, Vec<Event>) {
        let mut s2 = *s;
        let mut both = [RawTurn::IDLE; 2];
        both[x] = mine;
        both[o] = theirs;
        let mut ev = Vec::new();
        e.step_events(&mut s2, &both, &mut ev);
        (s2, ev)
    };
    let took = |was: &State, now: &State, f: usize| now.fighters[f].power_until != NO_BUFF && (was.fighters[f].power_until == NO_BUFF || now.fighters[f].power_until > was.fighters[f].power_until);
    let knocked = |ev: &[Event], f: usize| ev.iter().any(|v| matches!(v, Event::Knockback { target, .. } if *target as usize == f + 1));

    pv_forget();
    let mut tracker = Tracker::for_engine(e.clone(), x, seed);
    let mut prev: Option<RawTurn> = None;
    for k in 0..a {
        let view = View::from_state(e, &states[k], x);
        let told: Vec<Value> = visible_events(e, &states[k], x, &evs[k]).iter().map(|ev| ev.to_json(e)).collect();
        tracker.observe(&view, &told, prev);
        if k + 2 >= a {
            let _ = decide(&tracker, &view, book);
            if book.tree_pv_ponder > 0 {
                pv_ponder(&tracker, book, book.tree_pv_ponder, None);
            }
        }
        prev = Some(turns[k][x]);
    }
    // who took more power pads in the stretch (the real game)
    let mut pads = [0i32; 2];
    for k in a..=b {
        for f in 0..2 {
            pads[f] += took(&states[k], &states[k + 1], f) as i32;
        }
    }
    let class = if pads[x] > pads[o] {
        0
    } else if pads[x] == pads[o] {
        1
    } else {
        2
    };
    sum.val[class][0] += 1.0;
    let appear = appears.iter().copied().find(|&p| p >= a && p <= b);
    for k in a..=b {
        let s = &states[k];
        let real_next = &states[k + 1];
        let view = View::from_state(e, s, x);
        let told: Vec<Value> = visible_events(e, s, x, &evs[k]).iter().map(|ev| ev.to_json(e)).collect();
        tracker.observe(&view, &told, prev);
        LAST_ROOT.with(|l| *l.borrow_mut() = None);
        let d = decide(&tracker, &view, book);
        let root = LAST_ROOT.with(|l| l.borrow().as_ref().filter(|r| r.root.tick == s.tick).map(|r| (if x == 0 { r.value } else { -r.value }, r.acts[x].clone(), r.acts[o].clone(), r.sims)));
        let searched = d.note == "pv" && root.is_some();
        // X's turn from the playbook and the real one, each with the other's real turn
        let (book_next, book_ev) = play(s, d.turn, turns[k][o]);
        let same = phys(book_next) == phys(*real_next);
        sum.own[0] += 1.0;
        sum.own[1] += same as u32 as f64;
        sum.own[2] += (book_next.fighters[x].hp - real_next.fighters[x].hp) as f64;
        sum.own[3] += (book_next.fighters[o].hp - real_next.fighters[o].hp) as f64;
        sum.own[4] += took(s, &book_next, x) as u32 as f64;
        sum.own[5] += took(s, real_next, x) as u32 as f64;
        sum.own[6] += knocked(&book_ev, x) as u32 as f64;
        sum.own[7] += knocked(&evs[k + 1], x) as u32 as f64;
        sum.own[11] += searched as u32 as f64;
        let near = |st: &State| power_pads.iter().filter(|(i, _)| s.pads[*i].available || s.pads[*i].respawn_tick - s.tick <= 10).map(|(_, p)| chebyshev(*p, st.fighters[x].pos)).min();
        if let (Some(db), Some(dr)) = (near(&book_next), near(real_next)) {
            sum.own[8] += 1.0;
            sum.own[9] += db as f64;
            sum.own[10] += dr as f64;
        }
        let mut share_note = String::new();
        if let Some((value, mine, theirs, sims)) = root.as_ref().filter(|_| searched) {
            for (at, slot) in [(a, 1), (appear.unwrap_or(usize::MAX), 3), (b, 5)] {
                if k == at {
                    sum.val[class][slot] += *value;
                    sum.val[class][slot + 1] += 1.0;
                }
            }
            // the other's real turn among the search's lines
            let (fo, fo2) = (s.fighters[o], real_next.fighters[o]);
            if fo.state == FState::Idle && fo.alive && theirs.len() > 1 {
                let fired = |i: Option<usize>| i.map(|i| fo.cd_attack[i] == 0 && fo2.cd_attack[i] > 0).unwrap_or(false);
                let kind = if fired(burst) {
                    0
                } else if fired(frost) {
                    1
                } else if fired(shot) {
                    2
                } else if fo.cd_dash == 0 && fo2.cd_dash > 0 {
                    3
                } else if evs[k + 1].iter().all(|v| !matches!(v, Event::Knockback { target, .. } if *target as usize == o + 1)) && fo2.pos != fo.pos {
                    4
                } else {
                    5
                };
                let all_lines: u32 = theirs.iter().map(|t| t.1).sum();
                let want = phys(*real_next);
                let mut on = 0u32;
                let mut listed = false;
                for t in theirs.iter() {
                    if phys(play(s, turns[k][x], t.0).0) == want {
                        on += t.1;
                        listed = true;
                    }
                }
                let share = on as f64 / all_lines.max(1) as f64;
                sum.opp[kind][0] += 1.0;
                sum.opp[kind][1] += share;
                sum.opp[kind][2] += (share < 0.05) as u32 as f64;
                sum.opp[kind][3] += !listed as u32 as f64;
                let then = &states[(k + AHEAD).min(states.len() - 1)];
                let swing = (then.fighters[x].hp.max(0) - s.fighters[x].hp) - (then.fighters[o].hp.max(0) - s.fighters[o].hp);
                let slot = if share < 0.05 { 4 } else { 6 };
                sum.opp[kind][slot] += swing as f64;
                sum.opp[kind][slot + 1] += 1.0;
                if let (Some((n, m)), Some(net), true) = (regret, net.as_ref(), view.enemy.is_some()) {
                    // a fresh search from the real position, then X's answers to the real turn
                    let mut search = victris_engine::pvsearch::Search::new(e, net, victris_engine::pvsearch::PvConfig { lead_at: book.tree_pv_lead, max_nodes: book.tree_pv_nodes as usize, ..Default::default() });
                    search.iterate(s, n);
                    let listed_real = search.root()[o].iter().map(|t| t.0).find(|t| phys(play(s, turns[k][x], *t).0) == want);
                    let got = listed_real.and_then(|t| search.answers(s, o, &t, m));
                    match got.as_ref().and_then(|g| g.iter().find(|a| a.0 == d.turn).map(|a| (a.2, victris_engine::pvsearch::Search::best_answer(g)))) {
                        Some((ours, (best_turn, best))) if ours > f32::MIN => {
                            let at = if share < 0.05 { 0 } else { 3 };
                            sum.regret[kind][at] += 1.0;
                            sum.regret[kind][at + 1] += (best - ours).max(0.0) as f64;
                            sum.regret[kind][at + 2] += (best_turn != d.turn) as u32 as f64;
                        }
                        _ => sum.regret_skipped += 1.0,
                    }
                }
                share_note = format!("{} {:.0}%{}", KINDS[kind], 100.0 * share, if listed { "" } else { " (not listed)" });
            }
            if let Some(name) = trace {
                let (me, op) = (s.fighters[x], s.fighters[o]);
                let lying: String = power_pads.iter().map(|(i, _)| if s.pads[*i].available { 'o' } else { '.' }).collect();
                let top = |list: &Vec<(RawTurn, u32, f64)>| {
                    let mut l = list.clone();
                    l.sort_by(|p, q| q.1.cmp(&p.1));
                    let all_lines: u32 = l.iter().map(|t| t.1).sum();
                    l.iter().take(3).map(|t| format!("{} {:.0}% {:+.2}", short(&marshal::session::turn_json(s.tick, &t.0, e)), 100.0 * t.1 as f64 / all_lines.max(1) as f64, t.2)).collect::<Vec<_>>().join(" | ")
                };
                println!(
                    "  {name:<10} t{:>3} pads {lying} X {:?} hp {}{} | other {:?} hp {}{} | value {:+.3} {} lines | sent {} real {}{} | X: {} || other real: {} [{}] thought: {}",
                    s.tick,
                    me.pos,
                    me.hp,
                    if me.power_until != NO_BUFF { "P" } else { "" },
                    op.pos,
                    op.hp,
                    if op.power_until != NO_BUFF { "P" } else { "" },
                    value,
                    sims,
                    short(&marshal::session::turn_json(s.tick, &d.turn, e)),
                    short(&marshal::session::turn_json(s.tick, &turns[k][x], e)),
                    if same { "" } else { " (differs)" },
                    top(mine),
                    short(&marshal::session::turn_json(s.tick, &turns[k][o], e)),
                    share_note,
                    top(theirs)
                );
            }
        } else if let Some(name) = trace {
            println!("  {name:<10} t{:>3} no learned search this tick ({})", s.tick, d.note);
        }
        if book.tree_pv_ponder > 0 && d.note == "pv" {
            pv_ponder(&tracker, book, book.tree_pv_ponder, None);
        }
        prev = Some(turns[k][x]);
    }
}

/// A turn's JSON without its tick, for a trace line.
fn short(v: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(o) = v.as_object() {
        for (k, val) in o {
            if k != "tick" {
                parts.push(format!("{k}:{}", val.to_string().replace('"', "")));
            }
        }
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(",")
    }
}
