//! gamestats: the anatomy of games between named bots, from ladder replays (workstream 4
//! of the plan: how specific opponents are beaten).
//!
//!   gamestats --bots A,B [--versus X,Y*] [--won | --lost] [--games] <replay dir or files>...
//!
//! For every game of a counted bot (optionally only those it won or lost) it adds up, for
//! the counted bot and for its opponent: damage dealt by source (shot, frostburst,
//! repulsor, wall slam), shots fired, bursts thrown, repulsors used, dashes, pads taken,
//! and the tick of the first hit. With --games it prints one line per game as well.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::map::PadKind;
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

const NAMES: [&str; 11] = ["shot dmg", "frost dmg", "repulsor dmg", "slam dmg", "shots", "bursts", "repulsors", "dashes", "power pads", "health pads", "stuns"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let versus: Option<Vec<String>> = opt("--versus").map(|s| s.split(',').map(str::to_string).collect());
    let is_versus = |name: &str| versus.as_ref().map(|v| v.iter().any(|p| p.strip_suffix('*').map(|pre| name.starts_with(pre)).unwrap_or(p == name))).unwrap_or(true);
    let only_won = args.iter().any(|a| a == "--won");
    let only_lost = args.iter().any(|a| a == "--lost");
    let per_game = args.iter().any(|a| a == "--games");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" || a == "--versus" {
            skip = true;
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let mut total = [[0u64; 11]; 2];
    let (mut n, mut wins, mut ticks_sum, mut first_hit_sum) = (0u32, 0u32, 0u64, 0u64);
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let counted: Vec<bool> = (0..2)
            .map(|i| replay.bots.get(i).map(|n| bots.contains(n)).unwrap_or(false) && replay.bots.get(1 - i).map(|n| is_versus(n)).unwrap_or(false))
            .collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        let mut s = e.new_match(replay.seed);
        let mut events = Vec::new();
        let mut st = [[0u64; 11]; 2];
        let mut first_hit: Option<usize> = None;
        let mut ticks = 0usize;
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            e.step_events(&mut s, &t.turns, &mut events);
            ticks = ti + 1;
            for ev in &events {
                match ev {
                    Event::Hit { attacker, attack, damage, .. } => {
                        let k = match e.rules.attacks[*attack as usize].kind {
                            AttackKind::Projectile { .. } => 0,
                            AttackKind::Area { .. } => 1,
                            AttackKind::Burst { .. } => 2,
                        };
                        st[*attacker as usize - 1][k] += *damage as u64;
                        first_hit.get_or_insert(ti);
                    }
                    Event::WallSlam { attacker, damage, .. } => st[*attacker as usize - 1][3] += *damage as u64,
                    Event::ProjectileSpawn { owner, .. } => st[*owner as usize - 1][4] += 1,
                    Event::AreaSpawn { owner, .. } => st[*owner as usize - 1][5] += 1,
                    Event::Repulse { owner, .. } => st[*owner as usize - 1][6] += 1,
                    Event::Dash { fighter, .. } => st[*fighter as usize - 1][7] += 1,
                    Event::Pickup { fighter, kind, .. } => st[*fighter as usize - 1][if *kind == PadKind::Power { 8 } else { 9 }] += 1,
                    Event::Stunned { fighter, .. } => st[1 - (*fighter as usize - 1)][10] += 1,
                    _ => {}
                }
            }
        }
        let winner = s.result.and_then(|r| r.winner).map(|w| (w - 1) as usize);
        for x in 0..2 {
            if !counted[x] {
                continue;
            }
            let won = winner == Some(x);
            if (only_won && !won) || (only_lost && won) {
                continue;
            }
            n += 1;
            wins += won as u32;
            ticks_sum += ticks as u64;
            first_hit_sum += first_hit.unwrap_or(ticks) as u64;
            for k in 0..11 {
                total[0][k] += st[x][k];
                total[1][k] += st[1 - x][k];
            }
            if per_game {
                println!(
                    "{} {} vs {} on {}: {} ticks, first hit at {} | {:?} | {:?}",
                    if won { "won " } else { "lost" },
                    replay.bots[x],
                    replay.bots[1 - x],
                    e.map.name,
                    ticks,
                    first_hit.unwrap_or(0),
                    st[x],
                    st[1 - x]
                );
            }
        }
    }
    let g = n.max(1) as f64;
    println!("{} games of {}; won {}; {:.0} ticks a game; first hit at tick {:.0}", n, bots.join(", "), wins, ticks_sum as f64 / g, first_hit_sum as f64 / g);
    println!("{:<14} {:>10} {:>10}", "per game", "the bot", "opponent");
    for k in 0..11 {
        println!("{:<14} {:>10.1} {:>10.1}", NAMES[k], total[0][k] as f64 / g, total[1][k] as f64 / g);
    }
}
