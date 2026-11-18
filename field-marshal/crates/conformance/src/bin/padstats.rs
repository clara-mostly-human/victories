//! padstats: who takes the pads and what that is worth, measured on ladder replays
//! (workstream 6 of the plan: value and resources).
//!
//!   padstats [--bots A,B,C] [--versus X,Y*] [--map NAME] <replay dir or files>...
//!
//! For every game of a counted bot it records who took the first power pad and the first
//! health pad and when, how many of each the two sides took in all, and who won. A power
//! pad multiplies damage by 3/2 for 50 ticks and first appears at tick 20, so the race for
//! it is the first contest of every game.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::events::Event;
use victris_engine::map::PadKind;
use victris_engine::replay::parse_replay;

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
    games: u32,
    wins: u32,
    ticks: u64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Option<Vec<String>> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect());
    let versus: Option<Vec<String>> = opt("--versus").map(|s| s.split(',').map(str::to_string).collect());
    let is_versus = |name: &str| versus.as_ref().map(|v| v.iter().any(|p| p.strip_suffix('*').map(|pre| name.starts_with(pre)).unwrap_or(p == name))).unwrap_or(true);
    let only_map: Option<String> = opt("--map");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" || a == "--versus" || a == "--map" {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let mut table: BTreeMap<String, Cell> = BTreeMap::new();
    // per counted fighter-game: pads taken by us and by them, [power, health]
    let (mut mine, mut theirs, mut n) = ([0u32; 2], [0u32; 2], 0u32);
    let mut first_tick_sum = [0u64; 2];
    let mut first_count = [0u32; 2];
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
        let counted: Vec<bool> = (0..2)
            .map(|i| {
                bots.as_ref().map(|b| replay.bots.get(i).map(|n| b.contains(n)).unwrap_or(false)).unwrap_or(true)
                    && replay.bots.get(1 - i).map(|n| is_versus(n)).unwrap_or(false)
            })
            .collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        let mut s = e.new_match(replay.seed);
        let mut events = Vec::new();
        // first pickup of each kind: (tick, fighter index); and counts by fighter
        let mut first: [Option<(usize, usize)>; 2] = [None, None];
        let mut taken = [[0u32; 2]; 2];
        let mut ticks = 0usize;
        for (ti, t) in replay.ticks.iter().enumerate() {
            if s.result.is_some() {
                break;
            }
            e.step_events(&mut s, &t.turns, &mut events);
            ticks = ti + 1;
            for ev in &events {
                if let Event::Pickup { fighter, kind, .. } = ev {
                    let k = if *kind == PadKind::Power { 0 } else { 1 };
                    let f = *fighter as usize - 1;
                    taken[f][k] += 1;
                    first[k].get_or_insert((ti, f));
                }
            }
        }
        let winner = s.result.and_then(|r| r.winner).map(|w| (w - 1) as usize);
        for x in 0..2 {
            if !counted[x] {
                continue;
            }
            n += 1;
            for k in 0..2 {
                mine[k] += taken[x][k];
                theirs[k] += taken[1 - x][k];
            }
            let won = winner == Some(x);
            for (k, label) in [(0, "first power pad"), (1, "first health pad")] {
                let who = match first[k] {
                    None => "nobody took it",
                    Some((ti, f)) => {
                        first_tick_sum[k] += ti as u64;
                        first_count[k] += 1;
                        if f == x { "we took it" } else { "they took it" }
                    }
                };
                let c = table.entry(format!("{label}: {who}")).or_default();
                c.games += 1;
                c.wins += won as u32;
                c.ticks += ticks as u64;
            }
            let c = table.entry("all games".to_string()).or_default();
            c.games += 1;
            c.wins += won as u32;
            c.ticks += ticks as u64;
        }
    }
    let g = n.max(1) as f64;
    println!("{n} counted fighter-games");
    println!(
        "pads a game: power ours {:.2} theirs {:.2}; health ours {:.2} theirs {:.2}; first power pad taken at tick {:.0} on average, first health pad at tick {:.0}",
        mine[0] as f64 / g,
        theirs[0] as f64 / g,
        mine[1] as f64 / g,
        theirs[1] as f64 / g,
        first_tick_sum[0] as f64 / first_count[0].max(1) as f64,
        first_tick_sum[1] as f64 / first_count[1].max(1) as f64
    );
    println!("{:<36} {:>6} {:>6} {:>10}", "", "games", "won", "avg ticks");
    for (k, c) in &table {
        println!("{:<36} {:>6} {:>5.0}% {:>10.0}", k, c.games, 100.0 * c.wins as f64 / c.games.max(1) as f64, c.ticks as f64 / c.games.max(1) as f64);
    }
}
