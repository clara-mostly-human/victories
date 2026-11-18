//! coverstats: how bots use walls as cover, measured on ladder replays (workstream 7 of
//! the plan).
//!
//!   coverstats [--bots A,B,C] <replay dir or files>...
//!
//! Per map, for the named bots (default: everyone): the share of the fight spent out of
//! the opponent's line of sight, how many shots are fired on the tick the shooter steps
//! out of cover (a peek), how those shots do, how often the shooter is back behind cover
//! within two ticks of firing, and what follows a dash that breaks the line of sight.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::geom::chebyshev;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::State;

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
struct Tally {
    games: u32,
    contact: u32,
    hidden: u32,
    shots: u32,
    shot_hits: u32,
    peeks: u32,
    peek_hits: u32,
    back_in_cover: u32,
    dashes: u32,
    cover_dashes: u32,
    lure_pursued: u32,
    lure_paid: u32,
    lure_cost: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bots: Option<Vec<String>> = args.iter().position(|a| a == "--bots").and_then(|i| args.get(i + 1)).map(|s| s.split(',').map(str::to_string).collect());
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let mut by_map: BTreeMap<String, Tally> = BTreeMap::new();
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let Some(ranged) = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. })) else { continue };
        let counted: Vec<bool> = (0..2).map(|i| bots.as_ref().map(|b| replay.bots.get(i).map(|n| b.contains(n)).unwrap_or(false)).unwrap_or(true)).collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        let sees = |s: &State, x: usize| e.map.has_line_of_sight(s.fighters[x].pos, s.fighters[1 - x].pos);
        // Play the game once, keeping every state and what each tick's events said.
        let mut states: Vec<State> = vec![e.new_match(replay.seed)];
        let mut shot_by: Vec<[Option<u32>; 2]> = Vec::new();
        let mut dashed: Vec<[bool; 2]> = Vec::new();
        let mut hit_ids: std::collections::HashSet<u32> = Default::default();
        let mut events = Vec::new();
        for t in &replay.ticks {
            let mut s = *states.last().unwrap();
            if s.result.is_some() {
                break;
            }
            e.step_events(&mut s, &t.turns, &mut events);
            let mut shots = [None, None];
            let mut d = [false, false];
            for ev in &events {
                match ev {
                    Event::ProjectileSpawn { projectile, owner, attack, .. } if *attack as usize == ranged => shots[(*owner - 1) as usize] = Some(*projectile),
                    Event::Dash { fighter, .. } => d[(*fighter - 1) as usize] = true,
                    Event::ProjectileEnd { projectile, reason: "hit", .. } => {
                        hit_ids.insert(*projectile);
                    }
                    _ => {}
                }
            }
            shot_by.push(shots);
            dashed.push(d);
            states.push(s);
        }
        let n = shot_by.len();
        let t = by_map.entry(e.map.name.clone()).or_default();
        t.games += 1;
        for x in 0..2 {
            if !counted[x] {
                continue;
            }
            for i in 0..n {
                let (before, after) = (&states[i], &states[i + 1]);
                if chebyshev(before.fighters[x].pos, before.fighters[1 - x].pos) <= 8 {
                    t.contact += 1;
                    if !sees(before, x) {
                        t.hidden += 1;
                    }
                }
                if let Some(id) = shot_by[i][x] {
                    t.shots += 1;
                    let hit = hit_ids.contains(&id);
                    t.shot_hits += hit as u32;
                    // A peek: no line of sight when the tick began, one after our movement.
                    if !sees(before, x) && sees(after, x) {
                        t.peeks += 1;
                        t.peek_hits += hit as u32;
                    }
                    if (i + 2..=i + 3).any(|j| j < states.len() && !sees(&states[j], x)) {
                        t.back_in_cover += 1;
                    }
                }
                if dashed[i][x] {
                    t.dashes += 1;
                    if sees(before, x) && !sees(after, x) {
                        // A dash that breaks the line of sight. Does the opponent follow, and who gets hurt next?
                        t.cover_dashes += 1;
                        let end = (i + 9).min(states.len() - 1);
                        let pursued = (i + 2..=end).any(|j| chebyshev(states[j].fighters[x].pos, states[j].fighters[1 - x].pos) <= 2);
                        t.lure_pursued += pursued as u32;
                        let (s0, s1) = (&states[i + 1], &states[end]);
                        if s0.fighters[1 - x].hp > s1.fighters[1 - x].hp {
                            t.lure_paid += 1;
                        }
                        if s0.fighters[x].hp > s1.fighters[x].hp {
                            t.lure_cost += 1;
                        }
                    }
                }
            }
        }
    }
    let pct = |a: u32, b: u32| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    println!("bots: {}", bots.map(|b| b.join(", ")).unwrap_or_else(|| "everyone".into()));
    println!(
        "{:<12} {:>6} {:>12} {:>7} {:>6} {:>8} {:>9} {:>14} {:>8} {:>12} {:>9} {:>14} {:>14}",
        "map", "games", "out of sight", "shots", "hit", "peeks", "peek hit", "back in cover", "dashes", "into cover", "pursued", "we hit after", "we were hit"
    );
    for (name, t) in &by_map {
        println!(
            "{:<12} {:>6} {:>11.0}% {:>7} {:>5.0}% {:>7.0}% {:>8.0}% {:>13.0}% {:>8} {:>11.0}% {:>8.0}% {:>13.0}% {:>13.0}%",
            name,
            t.games,
            pct(t.hidden, t.contact),
            t.shots,
            pct(t.shot_hits, t.shots),
            pct(t.peeks, t.shots),
            pct(t.peek_hits, t.peeks),
            pct(t.back_in_cover, t.shots),
            t.dashes,
            pct(t.cover_dashes, t.dashes),
            pct(t.lure_pursued, t.cover_dashes),
            pct(t.lure_paid, t.cover_dashes),
            pct(t.lure_cost, t.cover_dashes)
        );
    }
}
