//! hurtcheck: where does the damage we take come from, and did the search see it coming?
//!
//!   hurtcheck <replay dir> --bot NAME [--iterations 27000] [--dash-burst] [--show 10]
//!
//! For every hit the named bot takes from a shot, a repulsor or a wall slam, finds the tick
//! on which the opponent played the turn that caused it (a shot's firing tick, the
//! repulsor's tick) and searches that position as the bot would (generation 1 judgment).
//! Reports, by kind of turn, how much damage came from turns the search gave a fair share
//! of the opponent's visits (foreseen) and from turns it gave almost none (a surprise),
//! and how much from turns that were not in its move list at all.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use victris_engine::events::Event;
use victris_engine::geom::chebyshev;
use victris_engine::mcts::{actions_with, Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::State;
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
use victris_engine::Engine;

fn same(a: &RawTurn, b: &RawTurn) -> bool {
    a.mv == b.mv && a.dash == b.dash && a.attack == b.attack
}

fn kind(e: &Engine, t: &RawTurn, opp: [i32; 2]) -> String {
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
                TargetIn::Tile(p) if !a.is_burst() => {
                    let off = chebyshev(p, opp);
                    if off == 0 { format!("{name} at us") } else { format!("{name} aimed {off} off") }
                }
                _ => name.to_string(),
            }
        }
        _ => "no attack".into(),
    };
    format!("{mv} + {atk}")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").expect("--bot NAME");
    let iterations: u32 = opt("--iterations").and_then(|s| s.parse().ok()).unwrap_or(27_000);
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(10);
    let dash_burst = args.iter().any(|a| a == "--dash-burst");
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let mut games = 0usize;
    // kind -> (damage foreseen, damage surprise, damage not in the list, count)
    let mut table: HashMap<String, (i32, i32, i32, usize)> = HashMap::new();
    let mut shown = 0usize;
    for path in files {
        let name = path.to_string_lossy().to_string();
        let mut text = String::new();
        if name.ends_with(".gz") {
            if std::fs::File::open(&path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else if name.ends_with(".jsonl") {
            text = std::fs::read_to_string(&path).unwrap_or_default();
        } else {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        games += 1;
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut history: Vec<(State, [RawTurn; 2])> = Vec::new();
        let mut spawned: HashMap<u32, usize> = HashMap::new(); // projectile id -> history index
        let mut events = Vec::new();
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let before = s;
            e.step_events(&mut s, &t.turns, &mut events);
            history.push((before, t.turns));
            let now = history.len() - 1;
            // Which of the opponent's turns hurt us this tick, and how much.
            let mut hurt: Vec<(usize, i32)> = Vec::new();
            let mut ended_hits: Vec<u32> = Vec::new();
            for ev in &events {
                match ev {
                    Event::ProjectileSpawn { projectile, owner, .. } if *owner as usize == 2 - me => {
                        spawned.insert(*projectile, now);
                    }
                    Event::ProjectileEnd { projectile, reason, .. } if *reason == "hit" => ended_hits.push(*projectile),
                    _ => {}
                }
            }
            for ev in &events {
                match ev {
                    Event::Hit { attacker, target, attack, damage, .. } if *target as usize == me + 1 && *attacker as usize == 2 - me => {
                        match e.rules.attacks[*attack as usize].kind {
                            AttackKind::Projectile { .. } => {
                                if let Some(i) = ended_hits.iter().find_map(|p| spawned.get(p)) {
                                    hurt.push((*i, *damage));
                                }
                            }
                            AttackKind::Burst { .. } => hurt.push((now, *damage)),
                            AttackKind::Area { .. } => {}
                        }
                    }
                    Event::WallSlam { attacker, target, damage } if *target as usize == me + 1 && *attacker as usize == 2 - me => hurt.push((now, *damage)),
                    _ => {}
                }
            }
            for (i, damage) in hurt {
                let (st, turns) = history[i];
                let theirs = turns[1 - me];
                let list = actions_with(e, &st, 1 - me, 0.25, false, false, false, dash_burst);
                let k = kind(e, &theirs, st.fighters[me].pos);
                let entry = table.entry(k.clone()).or_default();
                entry.3 += 1;
                if !list.iter().any(|a| same(&a.turn, &theirs)) {
                    entry.2 += damage;
                    continue;
                }
                let cfg = Config { value_weight: 1.0, value_gen: 1, buff_value: 0.0, dash_burst, ..Default::default() };
                let mut search = Search::new(e, cfg);
                search.start();
                search.iterate(&st, [None, None], iterations);
                let root = search.result();
                let total: u32 = root.acts[1 - me].iter().map(|a| a.1).sum();
                let n = root.acts[1 - me].iter().find(|a| same(&a.0, &theirs)).map(|a| a.1).unwrap_or(0);
                let share = n as f64 / total.max(1) as f64;
                if share >= 0.1 {
                    entry.0 += damage;
                } else {
                    entry.1 += damage;
                    if shown < show {
                        shown += 1;
                        let (f, o) = (&st.fighters[me], &st.fighters[1 - me]);
                        println!("  surprise: {} tick {}: they played {} ({:.0}% of their visits), {} apart, us {:?} them {:?}, {} damage", e.map.name, st.tick, k, 100.0 * share, chebyshev(f.pos, o.pos), f.pos, o.pos, damage);
                    }
                }
            }
        }
    }
    println!("\n{games} games of {bot}; damage taken from shots, repulsors and slams, by the turn that caused it:");
    println!("  {:<32} {:>6} {:>10} {:>10} {:>12}", "their turn", "hits", "foreseen", "surprise", "not in list");
    let mut rows: Vec<_> = table.into_iter().collect();
    rows.sort_by(|a, b| (b.1 .0 + b.1 .1 + b.1 .2).cmp(&(a.1 .0 + a.1 .1 + a.1 .2)));
    let (mut a, mut b, mut c) = (0, 0, 0);
    for (k, (f, s, n, cnt)) in rows {
        a += f;
        b += s;
        c += n;
        println!("  {:<32} {:>6} {:>10} {:>10} {:>12}", k, cnt, f, s, n);
    }
    println!("  {:<32} {:>6} {:>10} {:>10} {:>12}", "all", "", a, b, c);
}
