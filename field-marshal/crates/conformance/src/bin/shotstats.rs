//! shotstats: what a ranged shot is worth, measured on ladder replays (workstream 1 of
//! the plan).
//!
//!   shotstats [--bots A,B,C] [--map NAME] [--fine] [--horizon 8] <replay dir or files>...
//!
//! For every tick where a fighter could act, had its shot ready and the opponent in sight
//! within 6 tiles, it records the situation (range, whether the target could step or
//! dash), whether the fighter fired, whether that shot hit, and the hp swing in the
//! fighter's favour over the next `horizon` ticks. Firing and holding are then compared
//! situation by situation. With --bots, only those bots' decisions are counted.
//!
//! The comparison is observational: bots choose when to fire, so the "fire minus hold"
//! column shows what firing was worth when they chose to, not what it would be worth
//! always. It is a guide for which shots to put to a ladder experiment.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::geom::chebyshev;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::FState;
use victris_engine::turn::{AttackIn, TargetIn};

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
    fired: u32,
    hit: u32,
    fire_swing: f64,
    held: u32,
    hold_swing: f64,
    lead: u32,
    lead_hit: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Option<Vec<String>> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect());
    let horizon: usize = opt("--horizon").and_then(|s| s.parse().ok()).unwrap_or(8);
    let only_map: Option<String> = opt("--map");
    // --fine: one row per exact distance and per tick the target still has to wait to step
    let fine = args.iter().any(|a| a == "--fine");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--fine" {
            continue;
        }
        if a == "--bots" || a == "--horizon" || a == "--map" {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let mut table: BTreeMap<String, Cell> = BTreeMap::new();
    let (mut games, mut decisions) = (0u32, 0u32);
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
        let counted: Vec<bool> = (0..2).map(|i| bots.as_ref().map(|b| replay.bots.get(i).map(|n| b.contains(n)).unwrap_or(false)).unwrap_or(true)).collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        games += 1;
        let mut s = e.new_match(replay.seed);
        let mut events = Vec::new();
        let mut hp: Vec<[i32; 2]> = Vec::new();
        // (tick index, fighter, situation key, fired, lead, projectile id)
        let mut rows: Vec<(usize, usize, String, bool, bool, Option<u32>)> = Vec::new();
        let mut hits: std::collections::HashSet<u32> = Default::default();
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            hp.push([s.fighters[0].hp, s.fighters[1].hp]);
            let before = s;
            e.step_events(&mut s, &t.turns, &mut events);
            for x in 0..2 {
                if !counted[x] {
                    continue;
                }
                let (f, o) = (&before.fighters[x], &before.fighters[1 - x]);
                if f.state != FState::Idle || f.cd_attack[ranged] != 0 || !can_see(&e.rules, &e.map, f.pos, o.pos) {
                    continue;
                }
                let d = chebyshev(f.pos, o.pos);
                if d > 6 {
                    continue;
                }
                let band = if d <= 2 { "1-2 tiles" } else if d <= 4 { "3-4 tiles" } else { "5-6 tiles" };
                let mobility = if o.state != FState::Idle {
                    "cannot act     "
                } else if o.cd_move > 0 {
                    "step spent     "
                } else {
                    "step ready     "
                };
                let dash = if o.state == FState::Idle && o.cd_dash == 0 { "dash ready" } else { "no dash   " };
                let key = if fine {
                    let wait = if o.state != FState::Idle { "cannot act".to_string() } else { format!("steps in {}", o.cd_move) };
                    // ticks of flight at two tiles a tick along the straight line
                    let (dx, dy) = ((f.pos[0] - o.pos[0]) as f64, (f.pos[1] - o.pos[1]) as f64);
                    let flight = ((dx * dx + dy * dy).sqrt() / 2.0).ceil() as i32;
                    format!("flight {flight} | target {wait:<10} | {dash}")
                } else {
                    format!("{band} | target {mobility} | {dash}")
                };
                let (fired, lead) = match t.turns[x].attack {
                    AttackIn::Named { attack, target } if attack as usize == ranged => (true, !matches!(target, TargetIn::Tile(p) if p == o.pos)),
                    _ => (false, false),
                };
                // The projectile this fighter launched this tick, if the engine accepted the shot.
                let id = events.iter().find_map(|ev| match ev {
                    Event::ProjectileSpawn { projectile, owner, attack, .. } if *owner as usize == x + 1 && *attack as usize == ranged => Some(*projectile),
                    _ => None,
                });
                rows.push((ti, x, key, fired && id.is_some(), lead, id));
            }
            for ev in &events {
                if let Event::ProjectileEnd { projectile, reason: "hit", .. } = ev {
                    hits.insert(*projectile);
                }
            }
        }
        hp.push([s.fighters[0].hp, s.fighters[1].hp]);
        for (ti, x, key, fired, lead, id) in rows {
            let j = (ti + horizon).min(hp.len() - 1);
            let swing = ((hp[ti][1 - x] - hp[j][1 - x]) - (hp[ti][x] - hp[j][x])) as f64;
            let c = table.entry(key).or_default();
            decisions += 1;
            if fired {
                c.fired += 1;
                c.fire_swing += swing;
                let hit = id.map(|p| hits.contains(&p)).unwrap_or(false);
                c.hit += hit as u32;
                if lead {
                    c.lead += 1;
                    c.lead_hit += hit as u32;
                }
            } else {
                c.held += 1;
                c.hold_swing += swing;
            }
        }
    }
    println!("{games} games, {decisions} ticks with the shot ready and the opponent in sight within 6 tiles; swing = hp gained on the opponent over the next {horizon} ticks");
    println!("{:<52} {:>6} {:>6} {:>5} {:>9} {:>6} {:>9} {:>10} {:>12}", "situation", "fired", "rate", "hit", "swing", "held", "swing", "fire-hold", "lead shots");
    for (k, c) in &table {
        if c.fired + c.held < 40 {
            continue;
        }
        let fs = if c.fired > 0 { c.fire_swing / c.fired as f64 } else { 0.0 };
        let hs = if c.held > 0 { c.hold_swing / c.held as f64 } else { 0.0 };
        println!(
            "{:<52} {:>6} {:>5.0}% {:>4.0}% {:>+9.1} {:>6} {:>+9.1} {:>+10.1} {:>5} at {:>3.0}%",
            k,
            c.fired,
            100.0 * c.fired as f64 / (c.fired + c.held) as f64,
            if c.fired > 0 { 100.0 * c.hit as f64 / c.fired as f64 } else { 0.0 },
            fs,
            c.held,
            hs,
            fs - hs,
            c.lead,
            if c.lead > 0 { 100.0 * c.lead_hit as f64 / c.lead as f64 } else { 0.0 }
        );
    }
}
