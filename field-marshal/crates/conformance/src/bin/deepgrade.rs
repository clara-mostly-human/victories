//! deepgrade: which of our moves would a much deeper search have played differently, and
//! what did each such move cost?
//!
//!   deepgrade <replay dir> --bot A,B,... [--shallow 50000] [--deep 600000] [--gen 6]
//!             [--dash-burst] [--threads 3] [--show 25] [--every 1] [--max-nodes 120000]
//!             [--deep-depth 12] [--deep-quiet 6] [--last N] [--cache dir]
//!
//! For every tick on which a named bot could act with its opponent in view, the position is
//! searched twice from the true state: as the bot searches on the ladder (`--shallow`
//! iterations) and much deeper (`--deep`). The deep search is the referee: the cost of the
//! move played is the deep search's value of its own best move minus its value of the move
//! played (hp, from the bot's side). Moves the shallow search agrees with the deep one on
//! cost nothing. Disagreements are split into "more search fixes it" (the shallow search
//! preferred the deep search's move, or its own choice differed from what was played) and
//! "the judgment gets it wrong" (shallow and deep searches both like the move played less
//! than... see the summary), and grouped by situation. Positions are searched from the
//! true state (no fog), with the same judgment row both times.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;
use victris_engine::geom::chebyshev;
use victris_engine::mcts::{Config, Search};
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, State, NO_BUFF};
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
use victris_engine::Engine;

fn same(a: &RawTurn, b: &RawTurn) -> bool {
    a.mv == b.mv && a.dash == b.dash && a.attack == b.attack
}

fn describe(e: &Engine, t: &RawTurn, opp: [i32; 2]) -> String {
    let mv = match (t.mv, t.dash) {
        (_, DirIn::Dir(_)) => "dash",
        (DirIn::Dir(_), _) => "step",
        _ => "hold",
    };
    let atk = match t.attack {
        AttackIn::Named { attack, target } => {
            let a = &e.rules.attacks[attack as usize];
            let name = match a.kind {
                AttackKind::Projectile { .. } => "shot",
                AttackKind::Area { .. } => "frost",
                AttackKind::Burst { .. } => "repulsor",
            };
            match target {
                TargetIn::Tile(p) if !a.is_burst() && chebyshev(p, opp) > 0 => format!("{name} {} off", chebyshev(p, opp)),
                _ => name.to_string(),
            }
        }
        _ => "no attack".into(),
    };
    format!("{mv} + {atk}")
}

/// A coarse description of the situation, for grouping.
fn situation(e: &Engine, s: &State, me: usize) -> String {
    let (f, o) = (&s.fighters[me], &s.fighters[1 - me]);
    let d = chebyshev(f.pos, o.pos);
    let cd = |x: &victris_engine::state::Fighter, pred: &dyn Fn(&AttackKind) -> bool| {
        e.rules.attacks.iter().enumerate().filter(|(_, a)| pred(&a.kind)).map(|(i, _)| x.cd_attack[i]).min().unwrap_or(99)
    };
    let shot = |x| cd(x, &|k| matches!(k, AttackKind::Projectile { .. })) == 0;
    let band = match d {
        0..=2 => "close (1-2)",
        3..=4 => "mid (3-4)",
        _ => "far (5+)",
    };
    let pad = e.map.pads.iter().enumerate().any(|(pi, def)| {
        def.kind == victris_engine::map::PadKind::Power
            && (s.pads[pi].available || s.pads[pi].respawn_tick - s.tick <= 10)
            && chebyshev(f.pos, def.pos).min(chebyshev(o.pos, def.pos)) <= 3
    });
    format!(
        "{band}, our shot {}, theirs {}{}{}",
        if shot(f) { "ready" } else { "spent" },
        if shot(o) { "ready" } else { "spent" },
        if pad { ", power pad near" } else { "" },
        if o.power_until != NO_BUFF { ", they are buffed" } else if f.power_until != NO_BUFF { ", we are buffed" } else { "" }
    )
}

struct Finding {
    cost: f64,
    deep_also_shallow: bool, // the shallow search's own pick was the deep search's pick
    file: String,
    tick: i32,
    played: String,
    deep_best: String,
    situation: String,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let names: Vec<String> = opt("--bot").expect("--bot NAME[,NAME...]").split(',').map(str::to_string).collect();
    let shallow: u32 = opt("--shallow").and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let deep: u32 = opt("--deep").and_then(|s| s.parse().ok()).unwrap_or(600_000);
    let gen: usize = opt("--gen").and_then(|s| s.parse().ok()).unwrap_or(6);
    let threads: usize = opt("--threads").and_then(|s| s.parse().ok()).unwrap_or(3);
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(25);
    let every: usize = opt("--every").and_then(|s| s.parse().ok()).unwrap_or(1).max(1);
    let dash_burst = args.iter().any(|a| a == "--dash-burst");
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    // The deep search's tree is capped (positions past the cap are judged as leaves), or
    // three deep searches at once would need gigabytes.
    let max_nodes: u32 = opt("--max-nodes").and_then(|s| s.parse().ok()).unwrap_or(120_000);
    let cfg = Config { value_weight: 1.0, value_gen: gen, buff_value: 0.0, dash_burst, max_nodes, ..Default::default() };
    // The referee can also look farther ahead than the ladder search (12 ticks of tree and 6
    // of play-out): losses to the top bots tend to turn over 60 ticks, not 18.
    let deep_depth: u32 = opt("--deep-depth").and_then(|s| s.parse().ok()).unwrap_or(cfg.depth);
    let deep_quiet: u32 = opt("--deep-quiet").and_then(|s| s.parse().ok()).unwrap_or(cfg.quiet);
    let deep_cfg = Config { depth: deep_depth, quiet: deep_quiet, ..cfg };
    // --last N: only the last N ticks of each game.
    let last: i32 = opt("--last").and_then(|s| s.parse().ok()).unwrap_or(0);
    let cache: Option<String> = opt("--cache");
    if let Some(d) = cache.as_ref() {
        let _ = std::fs::create_dir_all(d);
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let findings: Mutex<Vec<Finding>> = Mutex::new(Vec::new());
    let totals: Mutex<(usize, usize, usize)> = Mutex::new((0, 0, 0)); // positions, played = deep best, shallow = deep best
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(path) = files.get(k) else { break };
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                // --cache dir: a game graded before (same settings) is read back, so a run
                // stopped by the batch pause carries on where it was.
                let cached = cache.as_ref().map(|d| format!("{d}/{name}.txt"));
                if let Some(text) = cached.as_ref().and_then(|c| std::fs::read_to_string(c).ok()) {
                    let mut lines = text.lines();
                    let head: Vec<usize> = lines.next().unwrap_or("").split('\t').filter_map(|v| v.parse().ok()).collect();
                    if head.len() == 3 {
                        let mut local = Vec::new();
                        for l in lines {
                            let v: Vec<&str> = l.split('\t').collect();
                            if v.len() == 6 {
                                local.push(Finding { cost: v[0].parse().unwrap_or(0.0), deep_also_shallow: v[1] == "1", file: name.clone(), tick: v[2].parse().unwrap_or(0), played: v[3].into(), deep_best: v[4].into(), situation: v[5].into() });
                            }
                        }
                        findings.lock().unwrap().extend(local);
                        let mut tt = totals.lock().unwrap();
                        tt.0 += head[0];
                        tt.1 += head[1];
                        tt.2 += head[2];
                        eprintln!("cached {name}");
                        continue;
                    }
                }
                let mut text = String::new();
                let ok = if name.ends_with(".gz") {
                    std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_ok()
                } else {
                    std::fs::read_to_string(path).map(|t| text = t).is_ok()
                };
                if !ok {
                    continue;
                }
                let Ok(replay) = parse_replay(&text) else { continue };
                let Some(me) = replay.bots.iter().position(|b| names.contains(b)) else { continue };
                let e = &replay.engine;
                let mut s = e.new_match(replay.seed);
                let end_tick = replay.ticks.len() as i32;
                let mut local: Vec<Finding> = Vec::new();
                let (mut pos, mut agree_played, mut agree_shallow) = (0usize, 0usize, 0usize);
                for (ti, t) in replay.ticks.iter().enumerate() {
                    if s.result.is_some() {
                        break;
                    }
                    let (f, o) = (&s.fighters[me], &s.fighters[1 - me]);
                    let in_window = last <= 0 || (ti as i32) >= end_tick - last;
                    if in_window && ti % every == 0 && f.state == FState::Idle && f.alive && o.alive && can_see(&e.rules, &e.map, f.pos, o.pos) {
                        let played = t.turns[me];
                        let mut d = Search::new(e, deep_cfg);
                        d.start();
                        d.iterate(&s, [None, None], deep);
                        let droot = d.result();
                        let mine = &droot.acts[me];
                        if let (Some(best), Some(pl)) = (mine.iter().max_by_key(|a| a.1).copied(), mine.iter().find(|a| same(&a.0, &played)).copied()) {
                            pos += 1;
                            let mut sh = Search::new(e, cfg);
                            sh.start();
                            sh.iterate(&s, [None, None], shallow);
                            let sroot = sh.result();
                            let spick = sroot.acts[me].iter().max_by_key(|a| a.1).map(|a| a.0);
                            let shallow_agrees = spick.map(|p| same(&p, &best.0)).unwrap_or(false);
                            agree_shallow += shallow_agrees as usize;
                            if same(&best.0, &played) {
                                agree_played += 1;
                            } else {
                                // the deep search's value of each, from our side
                                let cost = best.2 - pl.2;
                                if cost > 0.0 {
                                    local.push(Finding {
                                        cost,
                                        deep_also_shallow: shallow_agrees,
                                        file: name.clone(),
                                        tick: s.tick,
                                        played: describe(e, &played, o.pos),
                                        deep_best: describe(e, &best.0, o.pos),
                                        situation: situation(e, &s, me),
                                    });
                                }
                            }
                        }
                    }
                    e.step(&mut s, &t.turns);
                }
                if let Some(c) = cached.as_ref() {
                    let mut out = format!("{pos}\t{agree_played}\t{agree_shallow}\n");
                    for f in &local {
                        out.push_str(&format!("{}\t{}\t{}\t{}\t{}\t{}\n", f.cost, f.deep_also_shallow as u8, f.tick, f.played, f.deep_best, f.situation));
                    }
                    let _ = std::fs::write(c, out);
                }
                findings.lock().unwrap().extend(local);
                let mut tt = totals.lock().unwrap();
                tt.0 += pos;
                tt.1 += agree_played;
                tt.2 += agree_shallow;
                eprintln!("done {name}: {pos} positions");
            });
        }
    });
    let mut all = findings.into_inner().unwrap();
    let (pos, agree_played, agree_shallow) = totals.into_inner().unwrap();
    let games = files.len();
    let total_cost: f64 = all.iter().map(|f| f.cost).sum();
    let fixable: f64 = all.iter().filter(|f| f.deep_also_shallow).map(|f| f.cost).sum();
    println!(
        "{games} games, {pos} positions graded (shallow {shallow}, deep {deep} iterations at depth {deep_depth} + play-out {deep_quiet}, judgment row {gen}{}{})",
        if dash_burst { ", dash + repulsor" } else { "" },
        if last > 0 { format!(", last {last} ticks of each game") } else { String::new() }
    );
    println!(
        "  the move played was the deep search's choice in {:.0}%; the shallow search (as on the ladder now) picks the deep choice in {:.0}%",
        100.0 * agree_played as f64 / pos.max(1) as f64,
        100.0 * agree_shallow as f64 / pos.max(1) as f64
    );
    println!(
        "  cost of the moves played, by the deep search: {:.0} hp in all, {:.1} a game; of that, {:.0} hp where today's shallow search already picks the deep move (fixed by the newer brain or by luck) and {:.0} hp where it does not (needs more search or a better judgment)",
        total_cost,
        total_cost / games.max(1) as f64,
        fixable,
        total_cost - fixable
    );
    // By situation: cost where today's shallow search still disagrees with the deep one.
    let mut by: HashMap<String, (f64, usize)> = HashMap::new();
    for f in all.iter().filter(|f| !f.deep_also_shallow) {
        let e = by.entry(f.situation.clone()).or_default();
        e.0 += f.cost;
        e.1 += 1;
    }
    let mut rows: Vec<_> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap());
    println!("\n  where today's search still disagrees with the deep one, by situation (hp, positions):");
    for (k, (c, n)) in rows.iter().take(14) {
        println!("    {:>6.0} hp {:>5}  {k}", c, n);
    }
    let mut pairs: HashMap<String, (f64, usize)> = HashMap::new();
    for f in all.iter().filter(|f| !f.deep_also_shallow) {
        let e = pairs.entry(format!("played {} | deep prefers {}", f.played, f.deep_best)).or_default();
        e.0 += f.cost;
        e.1 += 1;
    }
    let mut prow: Vec<_> = pairs.into_iter().collect();
    prow.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap());
    println!("\n  the same, by move played and move preferred:");
    for (k, (c, n)) in prow.iter().take(14) {
        println!("    {:>6.0} hp {:>5}  {k}", c, n);
    }
    all.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap());
    println!("\n  the costliest single moves:");
    for f in all.iter().take(show) {
        println!(
            "    {:>5.1} hp  {} tick {:>3}: played {}, deep prefers {}{} | {}",
            f.cost,
            f.file,
            f.tick,
            f.played,
            f.deep_best,
            if f.deep_also_shallow { " (today's search agrees)" } else { "" },
            f.situation
        );
    }
}
