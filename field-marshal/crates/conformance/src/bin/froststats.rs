//! froststats: how frostbursts are used and what they lead to, measured on ladder replays
//! (workstream 3 of the plan).
//!
//!   froststats [--bots A,B,C] [--received] [--map NAME] [--horizon 10] <replay dir or files>...
//!
//! For every frostburst a counted fighter launches it records the situation at launch
//! (range to the target, whether the target's dash was ready, how far the burst was
//! placed from the target, whether the launcher's shot would be ready before the burst
//! goes off) and what followed before and just after the explosion: did the burst hit,
//! did the target dash, was the target slowed (it stepped inside the area), did one of
//! the launcher's shots hit, and the hp swing in the launcher's favour over `horizon`
//! ticks. With --received it counts the bursts thrown at the counted bots instead (the
//! swing is still the launcher's). Observational, like shotstats: a guide for
//! experiments, not a verdict.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::geom::chebyshev;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;

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

#[derive(Default, Clone, Copy)]
struct Cell {
    n: u32,
    hit: u32,
    dashed: u32,
    slowed: u32,
    shot_hit: u32,
    swing: f64,
    /// dashes by the target, by ticks since the launch (index 0 = the launch tick)
    dash_at: [u32; 8],
}

struct Launch {
    tick: usize,
    x: usize,
    keys: Vec<String>,
    explodes: usize,
    hit: bool,
    dashed: bool,
    slowed: bool,
    shot_hit: bool,
    dash_at: Option<usize>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Option<Vec<String>> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect());
    let horizon: usize = opt("--horizon").and_then(|s| s.parse().ok()).unwrap_or(10);
    let only_map: Option<String> = opt("--map");
    let received = args.iter().any(|a| a == "--received");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--received" {
            continue;
        }
        if a == "--bots" || a == "--horizon" || a == "--map" {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let mut table: BTreeMap<String, Cell> = BTreeMap::new();
    let (mut games, mut fighter_games) = (0u32, 0u32);
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        if only_map.as_ref().is_some_and(|m| *m != e.map.name) {
            continue;
        }
        let Some(ranged) = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. })) else { continue };
        let Some(frost) = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Area { .. })) else { continue };
        let AttackKind::Area { fuse, .. } = e.rules.attacks[frost].kind else { continue };
        let counted: Vec<bool> = (0..2).map(|i| bots.as_ref().map(|b| replay.bots.get(i).map(|n| b.contains(n)).unwrap_or(false)).unwrap_or(true)).collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        games += 1;
        fighter_games += counted.iter().filter(|c| **c).count() as u32;
        let mut s = e.new_match(replay.seed);
        let mut events = Vec::new();
        let mut hp: Vec<[i32; 2]> = Vec::new();
        let mut launches: Vec<Launch> = Vec::new();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            hp.push([s.fighters[0].hp, s.fighters[1].hp]);
            let before = s;
            e.step_events(&mut s, &t.turns, &mut events);
            // What this tick did for bursts already in the air (and a shot or two after).
            for l in launches.iter_mut() {
                let o = 1 - l.x;
                if ti <= l.explodes {
                    if s.fighters[o].cd_move >= 3 {
                        l.slowed = true;
                    }
                    for ev in &events {
                        match ev {
                            Event::Dash { fighter, .. } if *fighter as usize == o + 1 => {
                                l.dashed = true;
                                l.dash_at.get_or_insert((ti - l.tick).min(7));
                            }
                            Event::Hit { attacker, attack, .. } if *attacker as usize == l.x + 1 && *attack as usize == frost => l.hit = true,
                            _ => {}
                        }
                    }
                }
                if ti > l.tick && ti <= l.explodes + 3 {
                    for ev in &events {
                        if let Event::Hit { attacker, attack, .. } = ev {
                            if *attacker as usize == l.x + 1 && *attack as usize == ranged {
                                l.shot_hit = true;
                            }
                        }
                    }
                }
            }
            for ev in &events {
                let Event::AreaSpawn { owner, attack, pos, .. } = ev else { continue };
                let x = *owner as usize - 1;
                if *attack as usize != frost || !counted[if received { 1 - x } else { x }] {
                    continue;
                }
                let (f, o) = (&before.fighters[x], &before.fighters[1 - x]);
                let d = chebyshev(f.pos, o.pos);
                let band = if d <= 2 { "1-2 tiles" } else if d <= 4 { "3-4 tiles" } else { "5+ tiles " };
                let dash = if o.cd_dash == 0 {
                    "dash ready           "
                } else if o.cd_dash <= fuse {
                    "dash back before blow"
                } else {
                    "dash gone throughout "
                };
                let off = chebyshev(*pos, o.pos);
                let placed = if off == 0 { "on the target".to_string() } else { format!("{} off the target", off.min(3)) };
                let shot = if f.cd_attack[ranged] <= fuse { "shot ready before it blows" } else { "shot not ready in time   " };
                launches.push(Launch {
                    tick: ti,
                    x,
                    keys: vec![
                        "all".to_string(),
                        format!("target {dash}"),
                        format!("range {band} | target {dash}"),
                        format!("placed {placed}"),
                        shot.to_string(),
                    ],
                    explodes: ti + fuse as usize,
                    hit: false,
                    dashed: false,
                    slowed: false,
                    shot_hit: false,
                    dash_at: None,
                });
            }
        }
        hp.push([s.fighters[0].hp, s.fighters[1].hp]);
        for l in launches {
            let j = (l.tick + horizon).min(hp.len() - 1);
            let swing = ((hp[l.tick][1 - l.x] - hp[j][1 - l.x]) - (hp[l.tick][l.x] - hp[j][l.x])) as f64;
            for k in &l.keys {
                let c = table.entry(k.clone()).or_default();
                c.n += 1;
                c.hit += l.hit as u32;
                c.dashed += l.dashed as u32;
                c.slowed += l.slowed as u32;
                c.shot_hit += l.shot_hit as u32;
                c.swing += swing;
                if let Some(at) = l.dash_at {
                    c.dash_at[at] += 1;
                }
            }
        }
    }
    println!("{games} games, {fighter_games} counted fighter-games; swing = hp gained on the opponent over the next {horizon} ticks");
    println!(
        "{:<44} {:>7} {:>8} {:>6} {:>12} {:>13} {:>9} {:>7}   {}",
        "situation at launch", "bursts", "per game", "hit", "target dash", "target slowed", "shot hit", "swing", "target's dash, ticks after launch 0..6"
    );
    let pct = |a: u32, b: u32| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    for (k, c) in &table {
        println!(
            "{:<44} {:>7} {:>8.2} {:>5.0}% {:>11.0}% {:>12.0}% {:>8.0}% {:>+7.1}   {:?}",
            k,
            c.n,
            c.n as f64 / fighter_games.max(1) as f64,
            pct(c.hit, c.n),
            pct(c.dashed, c.n),
            pct(c.slowed, c.n),
            pct(c.shot_hit, c.n),
            c.swing / c.n.max(1) as f64,
            &c.dash_at[..7]
        );
    }
}
