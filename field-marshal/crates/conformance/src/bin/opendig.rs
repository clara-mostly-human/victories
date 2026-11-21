//! opendig: what a bot does around the first power-up on one map, split by who took it.
//!
//!   opendig <replay dirs> --bot NAME --map columns-21 [--show 2]
//!
//! Window A: tick 10 to the first pickup. Window B: the first pickup to the end of that buff.
//! Per window and per split (the bot took the first power-up / the opponent did): damage dealt
//! and taken by kind (shot, frost, repulsor, slam, collision), slams, mean distance between the
//! two, the share of ticks they could not see each other, pickups of the other power pad and of
//! health, hp difference at the end of B. Then a tick-by-tick trace of `--show` games where the
//! opponent took the first power-up and the bot was still 20+ hp ahead when the buff ran out.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::geom::chebyshev;
use victris_engine::map::PadKind;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::turn::{AttackIn, DirIn, RawTurn};
use victris_engine::Engine;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl.gz") || path.to_string_lossy().ends_with(".jsonl") {
        out.push(path.to_path_buf());
    }
}

const KINDS: [&str; 5] = ["shot", "frost", "repulsor", "slam", "collision"];

#[derive(Default, Clone, Copy)]
struct W {
    ticks: f64,
    dealt: [f64; 5],
    taken: [f64; 5],
    dist: f64,
    blind: f64,
}

fn describe(e: &Engine, t: &RawTurn) -> String {
    let mut out = Vec::new();
    if let DirIn::Dir(d) = t.mv {
        out.push(format!("step {}", victris_engine::geom::DIR_NAMES[d as usize]));
    }
    if let DirIn::Dir(d) = t.dash {
        out.push(format!("dash {}", victris_engine::geom::DIR_NAMES[d as usize]));
    }
    if let AttackIn::Named { attack, .. } = t.attack {
        out.push(e.rules.attacks[attack as usize].name.clone());
    }
    if out.is_empty() {
        "hold".into()
    } else {
        out.join(" + ")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").expect("--bot NAME");
    let map = opt("--map").unwrap_or_else(|| "columns-21".into());
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(2);
    // --below: show games where the bot was 20+ hp behind when the buff ran out instead
    let below = args.iter().any(|a| a == "--below");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if ["--bot", "--map", "--show"].contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    // [split: 0 bot took first, 1 opponent took first][window A, B]
    let mut acc = [[W::default(); 2]; 2];
    let mut n = [0usize; 2];
    let mut wins = [0usize; 2];
    let mut hp_end = [0.0f64; 2];
    let mut other_pad = [[0usize; 2]; 2]; // [split][bot, opponent] took the other power pad in B
    let mut heals = [[0usize; 2]; 2];
    let mut shown = 0usize;
    for path in &files {
        let mut text = String::new();
        if path.to_string_lossy().ends_with(".gz") {
            if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else {
            text = std::fs::read_to_string(path).unwrap_or_default();
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        if e.map.name != map {
            continue;
        }
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        let o = 1 - me;
        let Some(pk) = e.rules.pickups.as_ref() else { continue };
        let duration = pk.power.as_ref().map(|p| p.duration).unwrap_or(50);
        let mut s = e.new_match(replay.seed);
        let mut events = Vec::new();
        let mut first: Option<(usize, i32)> = None;
        let mut w = [W::default(); 2];
        let mut op = [0usize; 2];
        let mut he = [0usize; 2];
        let mut end_diff: Option<f64> = None;
        let mut trace: Vec<String> = Vec::new();
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let tick = s.tick;
            let win = match first {
                None if tick >= 10 => Some(0),
                Some((_, t0)) if tick < t0 + duration => Some(1),
                _ => None,
            };
            let (fm, fo) = (&s.fighters[me], &s.fighters[o]);
            if tick >= 12 && first.map(|(_, t0)| tick < t0 + duration).unwrap_or(true) {
                trace.push(format!(
                    "    t{:>3} us {:?} hp {:>3} cd s{} d{} | them {:?} hp {:>3} cd s{} d{} | {} apart{} | us: {} | them: {}",
                    tick, fm.pos, fm.hp, fm.cd_move, fm.cd_dash, fo.pos, fo.hp, fo.cd_move, fo.cd_dash,
                    chebyshev(fm.pos, fo.pos),
                    if can_see(&e.rules, &e.map, fm.pos, fo.pos) { "" } else { ", no sight" },
                    describe(e, &t.turns[me]), describe(e, &t.turns[o])
                ));
            }
            if let Some(k) = win {
                w[k].ticks += 1.0;
                w[k].dist += chebyshev(fm.pos, fo.pos) as f64;
                w[k].blind += (!can_see(&e.rules, &e.map, fm.pos, fo.pos)) as usize as f64;
            }
            e.step_events(&mut s, &t.turns, &mut events);
            let mut notes = Vec::new();
            for ev in &events {
                match ev {
                    Event::Hit { attacker, target, attack, damage, .. } => {
                        let kind = match e.rules.attacks[*attack as usize].kind {
                            AttackKind::Projectile { .. } => 0,
                            AttackKind::Area { .. } => 1,
                            _ => 2,
                        };
                        if let Some(k) = win {
                            if *attacker as usize == me + 1 { w[k].dealt[kind] += *damage as f64 } else if *target as usize == me + 1 { w[k].taken[kind] += *damage as f64 }
                        }
                        notes.push(format!("{} hits {} with {} for {}", if *attacker as usize == me + 1 { "we" } else { "they" }, if *target as usize == me + 1 { "us" } else { "them" }, KINDS[kind], damage));
                    }
                    Event::WallSlam { attacker, damage, .. } => {
                        if let Some(k) = win {
                            if *attacker as usize == me + 1 { w[k].dealt[3] += *damage as f64 } else { w[k].taken[3] += *damage as f64 }
                        }
                        notes.push(format!("{} slam for {}", if *attacker as usize == me + 1 { "we" } else { "they" }, damage));
                    }
                    Event::Collision { attacker, damage, .. } => {
                        if let Some(k) = win {
                            if *attacker as usize == me + 1 { w[k].dealt[4] += *damage as f64 } else { w[k].taken[4] += *damage as f64 }
                        }
                    }
                    Event::Pickup { fighter, kind, .. } => {
                        let who = *fighter as usize - 1;
                        notes.push(format!("{} pick up {:?}", if who == me { "we" } else { "they" }, kind));
                        if *kind == PadKind::Power {
                            if first.is_none() {
                                first = Some((who, s.tick));
                            } else if let Some((_, t0)) = first {
                                if s.tick < t0 + duration {
                                    op[(who != me) as usize] += 1;
                                }
                            }
                        } else if let Some((_, t0)) = first {
                            if s.tick < t0 + duration {
                                he[(who != me) as usize] += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !notes.is_empty() {
                if let Some(l) = trace.last_mut() {
                    l.push_str(&format!("  => {}", notes.join("; ")));
                }
            }
            if let Some((_, t0)) = first {
                if end_diff.is_none() && s.tick >= t0 + duration {
                    end_diff = Some((s.fighters[me].hp.max(0) - s.fighters[o].hp.max(0)) as f64);
                }
            }
        }
        let Some((taker, _)) = first else { continue };
        let split = (taker != me) as usize;
        n[split] += 1;
        let diff = end_diff.unwrap_or((s.fighters[me].hp.max(0) - s.fighters[o].hp.max(0)) as f64);
        hp_end[split] += diff;
        wins[split] += (s.result.and_then(|r| r.winner) == Some(me as u8 + 1)) as usize;
        for k in 0..2 {
            let a = &mut acc[split][k];
            a.ticks += w[k].ticks;
            a.dist += w[k].dist;
            a.blind += w[k].blind;
            for j in 0..5 {
                a.dealt[j] += w[k].dealt[j];
                a.taken[j] += w[k].taken[j];
            }
            other_pad[split][k] += op[k];
            heals[split][k] += he[k];
        }
        if split == 1 && (if below { diff <= -20.0 } else { diff >= 20.0 }) && shown < show {
            shown += 1;
            println!("== {} vs {} ({}), the opponent took the first power-up, we were {:+} when the buff ran out", bot, replay.bots[o], path.file_name().unwrap().to_string_lossy(), diff);
            for l in &trace {
                println!("{l}");
            }
        }
    }
    println!("\n{bot} on {map}");
    for split in 0..2 {
        let g = n[split].max(1) as f64;
        println!(
            "  {} took the first power-up: {} games, won {:.0}%, hp difference when the buff ran out {:+.1}; during the buff the other power pad went to us {:.2} / them {:.2}, health to us {:.2} / them {:.2} a game",
            if split == 0 { "we" } else { "they" },
            n[split], 100.0 * wins[split] as f64 / g, hp_end[split] / g,
            other_pad[split][0] as f64 / g, other_pad[split][1] as f64 / g, heals[split][0] as f64 / g, heals[split][1] as f64 / g
        );
        for (k, name) in ["  tick 10 to the pickup", "  during the buff     "].iter().enumerate() {
            let a = &acc[split][k];
            let tk = a.ticks.max(1.0);
            println!(
                "  {name}: {:.1} ticks, {:.1} apart, no sight {:.0}% | dealt {} | taken {}",
                a.ticks / g, a.dist / tk, 100.0 * a.blind / tk,
                (0..5).map(|j| format!("{} {:.1}", KINDS[j], a.dealt[j] / g)).collect::<Vec<_>>().join(", "),
                (0..5).map(|j| format!("{} {:.1}", KINDS[j], a.taken[j] / g)).collect::<Vec<_>>().join(", ")
            );
        }
    }
}
