//! posbench: the position bench. Fixed roots from held-out games, each with a deep search's
//! verdict on every turn, and a report of how a candidate search decides on them.
//!
//!   posbench teach <replay dirs> --top A,B,... --out file [--shard k/n] [--deep 40000]
//!            [--losses 2000] [--others 1000]
//!   posbench eval <replay dirs> --bench file... [--threads 6] [--iters 3000] [--lin 1] [--gen 13]
//!            [--net-value 0] [--net-cap 0] [--own-priors] [--plans] [--plan-playout] [--label name]
//!
//! Roots: games held out from training (a hash of the file name, % 10 < 2). First our bots'
//! losses to the top bots (`--top`), every 15 ticks where our fighter can act; then other held-out
//! games, every 30 ticks, until `--others`. The teacher searches each root deeply (depth 24 +
//! quiet 10) and keeps, for the side to judge, every turn's visits and average result.
//!
//! eval: the candidate searches each root with fixed iterations and plays its most visited turn.
//! Regret = the teacher's best average result minus the teacher's average result for that turn
//! (a turn the teacher visited fewer than 30 times counts as its worst well-visited turn), in hp.
//! Agreement = the candidate's turn is the teacher's best. Habits: how often the turn fires a
//! ready shot, a frostburst, a repulsor, a dash. Also, for the opponent at the root: how far it
//! really ends up from where it stands, and from where its last step would take it, 5 and 10
//! ticks later (the yardstick for goal-inference opponent models).

use flate2::read::GzDecoder;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use victris_engine::geom::{chebyshev, DIR_NAMES};
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::{parse_replay, Replay};
use victris_engine::state::{FState, State};
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
use victris_engine::Engine;

const OURS: [&str; 5] = ["field-marshal", "foot-soldier", "shock-trooper", "quarter-master", "spy-master"];

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

fn fnv(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn load(path: &Path) -> Option<Replay> {
    let mut text = String::new();
    GzDecoder::new(std::fs::File::open(path).ok()?).read_to_string(&mut text).ok()?;
    parse_replay(&text).ok()
}

fn turn_json(e: &Engine, t: &RawTurn) -> Value {
    let mut o = json!({});
    if let DirIn::Dir(d) = t.mv {
        o["move"] = json!(DIR_NAMES[d as usize]);
    }
    if let DirIn::Dir(d) = t.dash {
        o["dash"] = json!(DIR_NAMES[d as usize]);
    }
    if let AttackIn::Named { attack, target } = t.attack {
        let name = &e.rules.attacks[attack as usize].name;
        o["attack"] = match target {
            TargetIn::Tile(p) => json!({ "name": name, "target": p }),
            _ => json!({ "name": name }),
        };
    }
    o
}

/// The roots: (file index, tick, side, group 0 = our loss to a top bot, 1 = other).
fn roots(files: &[PathBuf], top: &[String], losses: usize, others: usize) -> Vec<(usize, i32, usize, u8)> {
    let mut lost = Vec::new();
    let mut rest = Vec::new();
    for (fi, path) in files.iter().enumerate() {
        if lost.len() >= losses && rest.len() >= others {
            break;
        }
        let Some(replay) = load(path) else { continue };
        let e = &replay.engine;
        let names: Vec<String> = (0..2).map(|i| replay.bots.get(i).cloned().unwrap_or_default()).collect();
        let ours: Vec<bool> = names.iter().map(|n| OURS.contains(&n.as_str())).collect();
        let tops: Vec<bool> = names.iter().map(|n| top.contains(n)).collect();
        let mut s = e.new_match(replay.seed);
        let mut seen: Vec<(i32, [bool; 2])> = Vec::new();
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            if s.tick >= 20 && s.fighters[0].alive && s.fighters[1].alive {
                seen.push((s.tick, [s.fighters[0].state == FState::Idle, s.fighters[1].state == FState::Idle]));
            }
            e.step(&mut s, &t.turns);
        }
        let Some(w) = s.result.and_then(|r| r.winner) else { continue };
        let winner = if w == 1 { 0 } else { 1 };
        // our loss to a top bot: our side, every 15 ticks
        let loss_side = (0..2).find(|&x| ours[x] && tops[1 - x] && winner != x);
        for (tick, idle) in &seen {
            if let Some(x) = loss_side {
                if lost.len() < losses && tick % 15 == 0 && idle[x] {
                    lost.push((fi, *tick, x, 0u8));
                }
            } else if rest.len() < others && tick % 30 == 0 {
                let x = ((*tick / 30) % 2) as usize;
                if idle[x] {
                    rest.push((fi, *tick, x, 1u8));
                }
            }
        }
    }
    lost.extend(rest);
    lost
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().cloned().unwrap_or_default();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let valued = ["--slam-value", "--loaded-value", "--win-shape", "--top", "--out", "--shard", "--deep", "--losses", "--others", "--bench", "--threads", "--iters", "--lin", "--gen", "--net-value", "--net-cap", "--label"];
    let mut files = Vec::new();
    let mut skip = false;
    for a in args.iter().skip(1) {
        if skip {
            skip = false;
            continue;
        }
        if valued.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| seen.insert(p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
    files.retain(|p| fnv(&p.file_name().unwrap().to_string_lossy()) % 10 < 2);
    if mode == "teach" {
        let top: Vec<String> = opt("--top").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
        let out = opt("--out").expect("--out file");
        let (shard, shards) = opt("--shard").and_then(|s| s.split_once('/').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(1)))).unwrap_or((0usize, 1usize));
        let deep = num("--deep", 40000.0) as u32;
        let list = roots(&files, &top, num("--losses", 2000.0) as usize, num("--others", 1000.0) as usize);
        eprintln!("{} roots ({} from our losses to top bots)", list.len(), list.iter().filter(|r| r.3 == 0).count());
        let cfg = Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, max_nodes: 80_000, depth: 24, quiet: 10, ..Default::default() };
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out).expect("open --out");
        let mut cur: Option<(usize, Replay)> = None;
        for (k, &(fi, tick, side, group)) in list.iter().enumerate() {
            if k % shards != shard {
                continue;
            }
            if cur.as_ref().map(|c| c.0) != Some(fi) {
                cur = load(&files[fi]).map(|r| (fi, r));
            }
            let Some((_, replay)) = &cur else { continue };
            let e = &replay.engine;
            let mut s = e.new_match(replay.seed);
            for t in &replay.ticks {
                if s.tick >= tick {
                    break;
                }
                e.step(&mut s, &t.turns);
            }
            let mut search = Search::new(e, cfg);
            search.start();
            search.iterate(&s, [None, None], deep);
            let res = search.result();
            let acts: Vec<Value> = res.acts[side].iter().map(|(t, n, q)| json!([turn_json(e, t), n, q])).collect();
            let line = json!({ "file": files[fi].file_name().unwrap().to_string_lossy(), "tick": tick, "side": side, "group": group, "acts": acts });
            let _ = writeln!(f, "{line}");
        }
        return;
    }
    if mode != "eval" {
        eprintln!("posbench teach|eval ...");
        return;
    }
    // eval
    let bench: Vec<Value> = args
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && args[i - 1] == "--bench")
        .flat_map(|(_, p)| std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect::<Vec<_>>())
        .collect();
    let by_name: std::collections::HashMap<String, PathBuf> = files.iter().map(|p| (p.file_name().unwrap().to_string_lossy().to_string(), p.clone())).collect();
    let cfg = Config {
        value_weight: num("--lin", 1.0),
        value_gen: num("--gen", 13.0) as usize,
        buff_value: 0.0,
        dash_burst: true,
        net_value: num("--net-value", 0.0),
        net_cap: num("--net-cap", 0.0),
        own_priors: args.iter().any(|a| a == "--own-priors"),
        plans: args.iter().any(|a| a == "--plans"),
        plan_playout: args.iter().any(|a| a == "--plan-playout"),
        win_shape: num("--win-shape", 0.0),
        slam_value: num("--slam-value", 0.0),
        loaded_value: num("--loaded-value", 0.0),
        ..Default::default()
    };
    let iters = num("--iters", 3000.0) as u32;
    let threads = num("--threads", 6.0) as usize;
    // group the roots by file
    let mut groups: std::collections::BTreeMap<String, Vec<&Value>> = std::collections::BTreeMap::new();
    for r in &bench {
        groups.entry(r["file"].as_str().unwrap_or("").to_string()).or_default().push(r);
    }
    let jobs: Vec<(&String, &Vec<&Value>)> = groups.iter().collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    // per group: n, regret sum, agree, ready, fired, frost, burst, dash, opp far5 stay, far5 step, far10 stay, far10 step, opp samples
    let totals: Vec<[f64; 15]> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads.max(1))
            .map(|_| {
                sc.spawn(|| {
                    let mut acc = vec![[0.0f64; 15]; 2];
                    loop {
                        let j = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some((name, roots)) = jobs.get(j) else { break };
                        let Some(path) = by_name.get(*name) else { continue };
                        let Some(replay) = load(path) else { continue };
                        let e = &replay.engine;
                        // states at every tick, to rebuild roots and see where the opponent went
                        let mut states: Vec<State> = Vec::new();
                        let mut s = e.new_match(replay.seed);
                        for t in &replay.ticks {
                            states.push(s);
                            if s.result.is_some() {
                                break;
                            }
                            e.step(&mut s, &t.turns);
                        }
                        states.push(s);
                        for r in roots.iter() {
                            let tick = r["tick"].as_i64().unwrap_or(0) as usize;
                            let side = r["side"].as_u64().unwrap_or(0) as usize;
                            let g = r["group"].as_u64().unwrap_or(1) as usize;
                            let Some(root) = states.get(tick) else { continue };
                            let teacher: Vec<(RawTurn, u32, f64)> = r["acts"].as_array().map(|a| a.iter().map(|x| (RawTurn::from_json(Some(&x[0]), &e.rules), x[1].as_u64().unwrap_or(0) as u32, x[2].as_f64().unwrap_or(0.0))).collect()).unwrap_or_default();
                            // the search keeps each side's results from that side's point of view
                            let sign = 1.0;
                            let well: Vec<&(RawTurn, u32, f64)> = teacher.iter().filter(|t| t.1 >= 30).collect();
                            if well.is_empty() {
                                continue;
                            }
                            let best = well.iter().map(|t| sign * t.2).fold(f64::MIN, f64::max);
                            let worst = well.iter().map(|t| sign * t.2).fold(f64::MAX, f64::min);
                            let best_turn = well.iter().max_by(|a, b| (sign * a.2).partial_cmp(&(sign * b.2)).unwrap()).map(|t| t.0);
                            let mut search = Search::new(e, cfg);
                            search.start();
                            search.iterate(root, [None, None], iters);
                            let res = search.result();
                            let Some(chosen) = res.acts[side].iter().max_by_key(|a| a.1).map(|a| a.0) else { continue };
                            let tq = teacher.iter().find(|t| t.0 == chosen && t.1 >= 30).map(|t| sign * t.2).unwrap_or(worst);
                            let a = &mut acc[g];
                            a[0] += 1.0;
                            a[1] += best - tq;
                            a[2] += (Some(chosen) == best_turn) as i32 as f64;
                            let f = &root.fighters[side];
                            let kind = |t: &RawTurn| match t.attack {
                                AttackIn::Named { attack, .. } => Some(&e.rules.attacks[attack as usize].kind),
                                _ => None,
                            };
                            let shot_i = e.rules.attacks.iter().position(|x| matches!(x.kind, victris_engine::rules::AttackKind::Projectile { .. }));
                            if let Some(si) = shot_i {
                                if f.cd_attack[si] == 0 {
                                    a[3] += 1.0;
                                    if matches!(chosen.attack, AttackIn::Named { attack, .. } if attack as usize == si) {
                                        a[4] += 1.0;
                                    }
                                }
                            }
                            match kind(&chosen) {
                                Some(victris_engine::rules::AttackKind::Area { .. }) => a[5] += 1.0,
                                Some(victris_engine::rules::AttackKind::Burst { .. }) => a[6] += 1.0,
                                _ => {}
                            }
                            if !matches!(chosen.dash, DirIn::None) {
                                a[7] += 1.0;
                            }
                            // the opponent's position 5 and 10 ticks on: from standing still, and from
                            // repeating its last step's direction
                            let o = 1 - side;
                            if let (Some(p5), Some(p10)) = (states.get(tick + 5), states.get(tick + 10)) {
                                let here = root.fighters[o].pos;
                                let before = states.get(tick.saturating_sub(2)).map(|b| b.fighters[o].pos).unwrap_or(here);
                                let v = [(here[0] - before[0]).signum(), (here[1] - before[1]).signum()];
                                let ahead = |k: i32| [here[0] + v[0] * k, here[1] + v[1] * k];
                                a[8] += chebyshev(here, p5.fighters[o].pos) as f64;
                                a[9] += chebyshev(ahead(2), p5.fighters[o].pos) as f64;
                                a[10] += chebyshev(here, p10.fighters[o].pos) as f64;
                                a[11] += chebyshev(ahead(5), p10.fighters[o].pos) as f64;
                                a[12] += 1.0;
                                // goal inference: its tiles over the last 8 ticks
                                let trail: Vec<victris_engine::geom::P> = (tick.saturating_sub(8)..=tick).filter_map(|k| states.get(k)).map(|st| st.fighters[o].pos).collect();
                                a[13] += chebyshev(victris_engine::goalmodel::forecast(e, root, o, &trail, 5, 0.4), p5.fighters[o].pos) as f64;
                                a[14] += chebyshev(victris_engine::goalmodel::forecast(e, root, o, &trail, 10, 0.4), p10.fighters[o].pos) as f64;
                            }
                        }
                    }
                    acc
                })
            })
            .collect();
        let mut t = vec![[0.0f64; 15]; 2];
        for h in hs {
            let a = h.join().unwrap();
            for g in 0..2 {
                for k in 0..15 {
                    t[g][k] += a[g][k];
                }
            }
        }
        t
    });
    let label = opt("--label").unwrap_or_else(|| "candidate".into());
    println!("posbench {label} ({iters} iterations)");
    for (g, name) in ["our losses to top bots", "other held-out roots"].iter().enumerate() {
        let a = &totals[g];
        let n = a[0].max(1.0);
        println!(
            "  {name:<24} {:>5} roots: regret {:.2} hp, agrees {:.1}% | fires a ready shot {:.1}%, frostburst {:.1}%, repulsor {:.1}%, dash {:.1}%",
            a[0] as usize,
            a[1] / n,
            100.0 * a[2] / n,
            100.0 * a[4] / a[3].max(1.0),
            100.0 * a[5] / n,
            100.0 * a[6] / n,
            100.0 * a[7] / n
        );
        if a[12] > 0.0 {
            println!("  {:<24} opponent's tile, error in tiles 5 / 10 ticks on: standing still {:.2} / {:.2}, last step {:.2} / {:.2}, inferred goal {:.2} / {:.2}", "", a[8] / a[12], a[10] / a[12], a[9] / a[12], a[11] / a[12], a[13] / a[12], a[14] / a[12]);
        }
    }
    let all: Vec<f64> = (0..15).map(|k| totals[0][k] + totals[1][k]).collect();
    println!("  all {:>5} roots: regret {:.2} hp, agrees {:.1}%", all[0] as usize, all[1] / all[0].max(1.0), 100.0 * all[2] / all[0].max(1.0));
}
