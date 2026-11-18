//! deepthink: an offline analyser. It replays games tick by tick and, at every tick where
//! a fighter could act, plays out each move it could have made many times on the exact
//! engine, to find the ticks where the move it actually made cost the most.
//!
//!   deepthink --bots A,B [--versus X,Y*] [--lost | --won] [--side us|them]
//!             [--engine tree|playouts] [--iterations 20000] [--sub 4000] [--depth 12]
//!             [--horizon 60] [--first 16] [--more 128] [--threads 3] [--top 30]
//!             [--games N] <replay dir or files>...
//!
//! The default engine is `tree`: a search in which both sides look for their best move at
//! every tick (`victris_engine::mcts`). A first search from the position gives the moves
//! worth comparing (our three most visited, plus the move really made) and the mix of
//! moves the opponent is likely to make. Each of our candidates is then searched again
//! against each of those opponent moves, and its value is the average over that mix.
//! Regret = best candidate's value minus the value of the move made.
//!
//! The older `playouts` engine is kept for comparison:
//! For the tick under study the other side plays the move it really played (both chose at
//! once, so that is what our move met). After that both sides follow the same default
//! policy for `horizon` ticks: the clone of the ladder's strong bots with three tactical
//! habits (`victris_engine::clone::tactical_turn`). A move's value is the average, over
//! the playouts, of hp taken minus hp lost, with 40 added for a kill and taken off for
//! dying. Every candidate gets `first` playouts; the best six and the move really made get
//! `more` on top. The same random numbers are used for every candidate, so differences are
//! not noise between seeds. Regret = best value minus the value of the move made.
//!
//! `--side them` studies the opponent of the named bots in the same games.
//!
//! What it cannot see: anything the default policy does not do well for either side.
//! Compare the average regret of the two sides before trusting a list.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::clone::tactical_turn;
use victris_engine::geom::{add, chebyshev, DIR_VEC};
use victris_engine::map::PadKind;
use victris_engine::mcts::{Config, Search};
use victris_engine::movefeat::{move_class, CLASS_NAMES};
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, State, NO_BUFF};
use victris_engine::turn::{AttackIn, DirIn, RawTurn};
use victris_engine::Engine;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl.gz") {
        out.push(path.to_path_buf());
    }
}

struct Mistake {
    regret: f64,
    line: String,
}

#[derive(Default)]
struct GameOut {
    mistakes: Vec<Mistake>,
    ticks: u32,
    regret: f64,
    kinds: BTreeMap<String, (u32, f64)>,
    /// ticks on which the search's first choice had the same kind of move, and the same
    /// kind of attack, as the move really made
    same_move: u32,
    same_attack: u32,
}

/// The tree engine's verdict on one tick: (best turn, its value, the value of the move
/// made, the search's first choice), values in hp for the fighter under study.
fn tree_tick(e: &Engine, s: &State, x: usize, actual: RawTurn, cfg: Config, sub: u32) -> (RawTurn, f64, f64, RawTurn) {
    let sign = if x == 0 { 1.0 } else { -1.0 };
    let mut search = Search::new(e, cfg);
    let root = search.run(s, [None, None]);
    let mut mine = root.acts[x].clone();
    mine.sort_by(|a, b| b.1.cmp(&a.1));
    let first_choice = mine[0].0;
    let mut cands: Vec<RawTurn> = mine.iter().take(3).map(|a| a.0).collect();
    if !cands.contains(&actual) {
        cands.push(actual);
    }
    // The opponent's likely moves: those with at least 8% of its visits, four at most.
    let mut theirs = root.acts[1 - x].clone();
    theirs.sort_by(|a, b| b.1.cmp(&a.1));
    let visits: u32 = theirs.iter().map(|a| a.1).sum();
    let mut mix: Vec<(RawTurn, f64)> = theirs.iter().take(4).filter(|a| a.1 * 100 >= visits * 8).map(|a| (a.0, a.1 as f64)).collect();
    if mix.is_empty() {
        mix.push((theirs[0].0, 1.0));
    }
    let total: f64 = mix.iter().map(|m| m.1).sum();
    let sub_cfg = Config { iterations: sub, depth: cfg.depth.saturating_sub(1).max(1), ..cfg };
    let value_of = |a: RawTurn| -> f64 {
        let mut v = 0.0;
        for (b, w) in &mix {
            let mut st = *s;
            let turns = if x == 0 { [a, *b] } else { [*b, a] };
            let lost = e.step(&mut st, &turns).hp_lost;
            let mut r = (lost[1] - lost[0]) as f64;
            if !st.fighters[1].alive {
                r += 40.0;
            }
            if !st.fighters[0].alive {
                r -= 40.0;
            }
            if st.result.is_none() {
                r += Search::new(e, sub_cfg).run(&st, [None, None]).value;
            }
            v += w / total * sign * r;
        }
        v
    };
    let values: Vec<f64> = cands.iter().map(|c| value_of(*c)).collect();
    let best = (0..cands.len()).max_by(|a, b| values[*a].partial_cmp(&values[*b]).unwrap()).unwrap();
    let at = cands.iter().position(|c| *c == actual).unwrap();
    (cands[best], values[best], values[at], first_choice)
}

fn attack_name(e: &Engine, t: &RawTurn) -> &'static str {
    match t.attack {
        AttackIn::Named { attack, .. } => match e.rules.attacks[attack as usize].kind {
            AttackKind::Projectile { .. } => "shot",
            AttackKind::Area { .. } => "frostburst",
            AttackKind::Burst { .. } => "repulsor",
        },
        _ => "no attack",
    }
}

fn describe(e: &Engine, s: &State, x: usize, t: &RawTurn) -> String {
    format!("{} + {}", CLASS_NAMES[move_class(s, x, t)], attack_name(e, t))
}

/// Where a step ends (a dash or a hold: where we stand).
fn step_end(s: &State, x: usize, t: &RawTurn) -> [i32; 2] {
    match (t.dash, t.mv) {
        (DirIn::Dir(_), _) => s.fighters[x].pos,
        (_, DirIn::Dir(d)) if s.fighters[x].cd_move == 0 => add(s.fighters[x].pos, DIR_VEC[d as usize]),
        _ => s.fighters[x].pos,
    }
}

#[allow(clippy::too_many_arguments)]
fn value(e: &Engine, s: &State, x: usize, mine: RawTurn, theirs: RawTurn, horizon: usize, seed: u64) -> f64 {
    let mut st = *s;
    let mut rng = seed | 1;
    let turns = if x == 0 { [mine, theirs] } else { [theirs, mine] };
    e.step(&mut st, &turns);
    for _ in 1..horizon {
        if st.result.is_some() {
            break;
        }
        let t0 = tactical_turn(e, &st, 0, &mut rng);
        let t1 = tactical_turn(e, &st, 1, &mut rng);
        e.step(&mut st, &[t0, t1]);
    }
    let (me0, op0) = (s.fighters[x].hp, s.fighters[1 - x].hp);
    let (me1, op1) = (st.fighters[x].hp.max(0), st.fighters[1 - x].hp.max(0));
    let mut v = (op0 - op1) as f64 - (me0 - me1) as f64;
    if !st.fighters[1 - x].alive {
        v += 40.0;
    }
    if !st.fighters[x].alive {
        v -= 40.0;
    }
    v
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, default: usize| opt(name).and_then(|s| s.parse().ok()).unwrap_or(default);
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let versus: Option<Vec<String>> = opt("--versus").map(|s| s.split(',').map(str::to_string).collect());
    let is_versus = |name: &str| versus.as_ref().map(|v| v.iter().any(|p| p.strip_suffix('*').map(|pre| name.starts_with(pre)).unwrap_or(p == name))).unwrap_or(true);
    let (only_won, only_lost) = (args.iter().any(|a| a == "--won"), args.iter().any(|a| a == "--lost"));
    let them = opt("--side").as_deref() == Some("them");
    let (horizon, first, more, threads, top, max_games) = (num("--horizon", 60), num("--first", 16), num("--more", 128), num("--threads", 3), num("--top", 30), num("--games", usize::MAX));
    let tree = opt("--engine").as_deref() != Some("playouts");
    let cfg = Config { iterations: num("--iterations", 20_000) as u32, depth: num("--depth", 12) as u32, ..Config::default() };
    let sub = num("--sub", 4_000) as u32;
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a.starts_with("--") {
            skip = !matches!(a.as_str(), "--won" | "--lost");
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    // The games to study: (replay text, index of the fighter under study).
    let mut games: Vec<(String, usize)> = Vec::new();
    for path in &files {
        if games.len() >= max_games {
            break;
        }
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        for i in 0..2 {
            let counted = replay.bots.get(i).map(|n| bots.contains(n)).unwrap_or(false) && replay.bots.get(1 - i).map(|n| is_versus(n)).unwrap_or(false);
            if !counted {
                continue;
            }
            let won = replay.result.as_ref().and_then(|r| r.winner) == Some(i as i64 + 1);
            if (only_won && !won) || (only_lost && won) {
                continue;
            }
            games.push((text.clone(), if them { 1 - i } else { i }));
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut outs: Vec<GameOut> = Vec::new();
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut mine: Vec<GameOut> = Vec::new();
                    loop {
                        let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((text, x)) = games.get(j) else { break };
                        mine.push(study(text, *x, j as u64, horizon, first, more, if tree { Some((cfg, sub)) } else { None }));
                    }
                    mine
                })
            })
            .collect();
        for h in handles {
            outs.extend(h.join().unwrap());
        }
    });
    let ticks: u32 = outs.iter().map(|o| o.ticks).sum();
    let (same_move, same_attack): (u32, u32) = (outs.iter().map(|o| o.same_move).sum(), outs.iter().map(|o| o.same_attack).sum());
    let regret: f64 = outs.iter().map(|o| o.regret).sum();
    let mut kinds: BTreeMap<String, (u32, f64)> = BTreeMap::new();
    let mut all: Vec<Mistake> = Vec::new();
    for o in outs {
        for (k, (n, r)) in o.kinds {
            let c = kinds.entry(k).or_default();
            c.0 += n;
            c.1 += r;
        }
        all.extend(o.mistakes);
    }
    let g = games.len().max(1) as f64;
    println!(
        "{} games of {}{}; {} ticks studied; regret {:.1} hp a game, {:.2} a tick; {} ticks with a regret of 4 or more ({:.1} a game)",
        games.len(),
        if them { "the opponents of " } else { "" },
        bots.join(", "),
        ticks,
        regret / g,
        regret / ticks.max(1) as f64,
        all.len(),
        all.len() as f64 / g
    );
    if tree {
        println!(
            "the search's first choice is the same kind of move as the one made on {:.0}% of ticks, and the same kind of attack on {:.0}%",
            100.0 * same_move as f64 / ticks.max(1) as f64,
            100.0 * same_attack as f64 / ticks.max(1) as f64
        );
    }
    println!("{:<52} {:>6} {:>10} {:>10}", "kind of mistake (should have -> did)", "count", "hp in all", "hp a game");
    let mut rows: Vec<_> = kinds.into_iter().collect();
    rows.sort_by(|a, b| b.1 .1.partial_cmp(&a.1 .1).unwrap());
    for (k, (n, r)) in rows.iter().take(24) {
        println!("{:<52} {:>6} {:>10.0} {:>10.1}", k, n, r, r / g);
    }
    all.sort_by(|a, b| b.regret.partial_cmp(&a.regret).unwrap());
    println!("the {} costliest ticks:", top.min(all.len()));
    for m in all.iter().take(top) {
        println!("  {:>5.1}  {}", m.regret, m.line);
    }
}

fn study(text: &str, x: usize, game_index: u64, horizon: usize, first: usize, more: usize, tree: Option<(Config, u32)>) -> GameOut {
    let mut out = GameOut::default();
    let Ok(replay) = parse_replay(text) else { return out };
    let e = &replay.engine;
    let mut s = e.new_match(replay.seed);
    for (ti, t) in replay.ticks.iter().enumerate() {
        if s.result.is_some() {
            break;
        }
        let (f, o) = (s.fighters[x], s.fighters[1 - x]);
        if f.state == FState::Idle && o.alive {
            let d = chebyshev(f.pos, o.pos);
            let seen = can_see(&e.rules, &e.map, f.pos, o.pos);
            let actual = t.turns[x];
            let (best_turn, q_best, q_actual) = if let Some((cfg, sub)) = tree {
                let (best, vb, va, first_choice) = tree_tick(e, &s, x, actual, cfg, sub);
                out.same_move += (move_class(&s, x, &first_choice) == move_class(&s, x, &actual)) as u32;
                out.same_attack += (attack_name(e, &first_choice) == attack_name(e, &actual)) as u32;
                (best, vb, va)
            } else {
                // Every move we could make, with every attack we could add.
                let mut moves = vec![RawTurn::IDLE];
                if f.cd_move == 0 {
                    for dir in 0..8u8 {
                        if !e.map.step_blocked(f.pos, dir) && add(f.pos, DIR_VEC[dir as usize]) != o.pos {
                            moves.push(RawTurn::step(dir));
                        }
                    }
                }
                if f.cd_dash == 0 {
                    for dir in 0..8u8 {
                        moves.push(RawTurn::dash(dir));
                    }
                }
                let mut cands: Vec<RawTurn> = Vec::new();
                for mv in &moves {
                    cands.push(*mv);
                    for (i, a) in e.rules.attacks.iter().enumerate() {
                        if f.cd_attack[i] != 0 {
                            continue;
                        }
                        match a.kind {
                            AttackKind::Projectile { .. } if seen => cands.push(mv.with_attack(i as u8, o.pos)),
                            AttackKind::Area { range, .. } if seen && d <= range => cands.push(mv.with_attack(i as u8, o.pos)),
                            AttackKind::Burst { .. } if d <= 3 => cands.push(mv.with_burst(i as u8)),
                            _ => {}
                        }
                    }
                }
                let actual_at = match cands.iter().position(|c| *c == actual) {
                    Some(i) => i,
                    None => {
                        cands.push(actual);
                        cands.len() - 1
                    }
                };
                let theirs = t.turns[1 - x];
                let seed = |i: usize| (game_index + 1).wrapping_mul(0x9e3779b97f4a7c15) ^ (ti as u64 + 1).wrapping_mul(0xbf58476d1ce4e5b9) ^ (i as u64 + 1).wrapping_mul(0x94d049bb133111eb);
                let mut sum: Vec<f64> = cands.iter().map(|c| (0..first).map(|i| value(e, &s, x, *c, theirs, horizon, seed(i))).sum()).collect();
                let mut n: Vec<usize> = vec![first; cands.len()];
                let mut order: Vec<usize> = (0..cands.len()).collect();
                order.sort_by(|a, b| sum[*b].partial_cmp(&sum[*a]).unwrap());
                let mut refine: Vec<usize> = order.into_iter().take(6).collect();
                if !refine.contains(&actual_at) {
                    refine.push(actual_at);
                }
                for &c in &refine {
                    sum[c] += (first..first + more).map(|i| value(e, &s, x, cands[c], theirs, horizon, seed(i))).sum::<f64>();
                    n[c] += more;
                }
                let q = |c: usize| sum[c] / n[c] as f64;
                let best = *refine.iter().max_by(|a, b| q(**a).partial_cmp(&q(**b)).unwrap()).unwrap();
                (cands[best], q(best), q(actual_at))
            };
            let regret = (q_best - q_actual).max(0.0);
            out.ticks += 1;
            out.regret += regret;
            if regret >= 4.0 {
                let (b, a) = (best_turn, actual);
                // A power pad we could have stepped onto and did not.
                let pad_at = |p: [i32; 2]| e.map.pads.iter().enumerate().any(|(i, def)| def.kind == PadKind::Power && def.pos == p && (s.pads[i].available || s.pads[i].respawn_tick - s.tick <= 1));
                let kind = if f.power_until == NO_BUFF && pad_at(step_end(&s, x, &b)) && !pad_at(step_end(&s, x, &a)) {
                    "step onto the power pad".to_string()
                } else if attack_name(e, &b) != attack_name(e, &a) {
                    format!("{} -> {}", attack_name(e, &b), attack_name(e, &a))
                } else {
                    format!("move {} -> {}", CLASS_NAMES[move_class(&s, x, &b)], CLASS_NAMES[move_class(&s, x, &a)])
                };
                let c = out.kinds.entry(kind.clone()).or_default();
                c.0 += 1;
                c.1 += regret;
                let shot_cd = |fighter: &victris_engine::state::Fighter| e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. })).map(|i| fighter.cd_attack[i]).unwrap_or(0);
                out.mistakes.push(Mistake {
                    regret,
                    line: format!(
                        "{} vs {} on {} tick {}: should {} (worth {:+.1}), did {} ({:+.1}) | {} tiles apart, hp {} vs {}, shot in {} vs {}, their dash in {}, their step in {}{}{}",
                        replay.bots[x],
                        replay.bots[1 - x],
                        e.map.name,
                        ti,
                        describe(e, &s, x, &b),
                        q_best,
                        describe(e, &s, x, &a),
                        q_actual,
                        d,
                        f.hp,
                        o.hp,
                        shot_cd(&f),
                        shot_cd(&o),
                        o.cd_dash,
                        o.cd_move,
                        if f.power_until != NO_BUFF { ", we are buffed" } else { "" },
                        if o.power_until != NO_BUFF { ", they are buffed" } else { "" }
                    ),
                });
            }
        }
        e.step(&mut s, &t.turns);
    }
    out
}
