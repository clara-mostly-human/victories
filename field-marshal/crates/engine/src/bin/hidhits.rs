//! hidhits: in arena replays, what a named playbook and its opponent lose hp to and what they
//! fire, by how long the two fighters have been out of each other's sight.
//!
//!   hidhits --bot A <replay dir or .jsonl files>...
//!
//! Every tick is put in one of five kinds by the state at its start: in sight; just out (in
//! sight, and out of sight at some tick in the last five); hidden for 1-5 ticks, 6-10 ticks,
//! 11 ticks or more. Per kind it prints the hp a game each side loses by what hit it, and the
//! attacks a game each side starts.

use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl") {
        out.push(path.to_path_buf());
    }
}

const KINDS: [&str; 5] = ["in sight", "just out", "hidden 1-5", "hidden 6-10", "hidden 11+"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bot = args.iter().position(|a| a == "--bot").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "A".into());
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bot" {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    // [kind of tick][side: 0 the named playbook, 1 its opponent][what: the rules' attacks, then slam, collision]
    let mut lost = [[[0f64; 8]; 2]; 5];
    let mut made = [[[0f64; 8]; 2]; 5];
    let mut ticks = [0u64; 5];
    let mut names: Vec<String> = Vec::new();
    let mut games = 0u32;
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|n| *n == bot) else { continue };
        let e = &replay.engine;
        if names.is_empty() {
            names = e.rules.attacks.iter().map(|a| a.name.clone()).collect();
            names.push("wall slam".into());
            names.push("collision".into());
        }
        let n = e.rules.attacks.len().min(6);
        games += 1;
        let mut s = e.new_match(replay.seed);
        let mut events = Vec::new();
        // ticks in a row out of sight, and ticks since the two last could not see each other
        let (mut out, mut since) = (0u32, 1000u32);
        let side = |fighter: u8| if fighter as usize - 1 == me { 0 } else { 1 };
        for t in replay.ticks.iter() {
            if s.result.is_some() {
                break;
            }
            let seen = can_see(&e.rules, &e.map, s.fighters[0].pos, s.fighters[1].pos);
            if seen {
                out = 0;
                since += 1;
            } else {
                out += 1;
                since = 0;
            }
            let kind = if seen {
                if since <= 5 { 1 } else { 0 }
            } else if out <= 5 {
                2
            } else if out <= 10 {
                3
            } else {
                4
            };
            ticks[kind] += 1;
            e.step_events(&mut s, &t.turns, &mut events);
            for ev in &events {
                match ev {
                    Event::Hit { target, attack, damage, .. } => lost[kind][side(*target)][(*attack as usize).min(n - 1)] += *damage as f64,
                    Event::WallSlam { target, damage, .. } => lost[kind][side(*target)][n] += *damage as f64,
                    Event::Collision { fighters, damage, .. } => {
                        for f in fighters {
                            lost[kind][side(*f)][n + 1] += *damage as f64;
                        }
                    }
                    Event::Attack { fighter, attack, .. } => made[kind][side(*fighter)][(*attack as usize).min(n - 1)] += 1.0,
                    _ => {}
                }
            }
        }
    }
    let g = games.max(1) as f64;
    println!("{games} games of {bot}; hp a game lost to each kind of hit, and attacks a game started, by kind of tick ({bot} / opponent)");
    for k in 0..5 {
        println!("== {} ({:.0} ticks a game)", KINDS[k], ticks[k] as f64 / g);
        let mut line = String::from("   lost: ");
        let (mut a, mut b) = (0.0, 0.0);
        for (i, name) in names.iter().enumerate() {
            line += &format!("{} {:.1} / {:.1}; ", name, lost[k][0][i] / g, lost[k][1][i] / g);
            a += lost[k][0][i];
            b += lost[k][1][i];
        }
        println!("{line}all {:.1} / {:.1}", a / g, b / g);
        let mut line = String::from("   started: ");
        for (i, name) in names.iter().enumerate().take(names.len().saturating_sub(2)) {
            line += &format!("{} {:.1} / {:.1}; ", name, made[k][0][i] / g, made[k][1][i] / g);
        }
        println!("{line}");
    }
}
