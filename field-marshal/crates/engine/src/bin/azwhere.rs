//! Where the fighters stood, tick by tick, for studying how well a hidden opponent's place can
//! be predicted.
//!
//!   azwhere --list FILES.txt [--map pillars-21] > ticks.tsv
//!   azwhere --az FILE.az --map pillars-21 > ticks.tsv
//!   azwhere --los pillars-21 > los.txt
//!
//! `--list`: for every replay a line `G index seed map bot0 bot1 winner` (winner 0, 1 or -1),
//! then a line a tick: `tick x0 y0 x1 y1 see hp0 hp1 power0 power1`, where `see` is 1 while the
//! fighters see each other and `power` is the ticks of power-up a fighter has left.
//! `--los`: the map's size, then for every tile (row by row) a row of 0/1: the tiles a fighter
//! standing there sees; walls are printed as rows of 2.

use std::io::Write;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::state::NO_BUFF;
use victris_engine::Engine;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = std::io::stdout();
    let mut out = std::io::BufWriter::new(out.lock());
    if let Some(m) = arg(&args, "--los") {
        let e = Engine::shipped(&m);
        let (w, h) = (e.map.w, e.map.h);
        writeln!(out, "{} {}", w, h).unwrap();
        for y in 0..h {
            for x in 0..w {
                let wall = e.map.is_wall([x, y]);
                let row: String = (0..h)
                    .flat_map(|by| (0..w).map(move |bx| (bx, by)))
                    .map(|(bx, by)| if wall { '2' } else if can_see(&e.rules, &e.map, [x, y], [bx, by]) { '1' } else { '0' })
                    .collect();
                writeln!(out, "{}", row).unwrap();
            }
        }
        return;
    }
    let map = arg(&args, "--map");
    let mut index = 0usize;
    // --az FILE.az --map MAP: saved self-play games (azgen's, or the arena's ARENA_AZ); both fighters are "self"
    if let Some(path) = arg(&args, "--az") {
        let m = map.clone().expect("--az needs --map");
        let e = Engine::shipped(&m);
        for g in victris_engine::azdata::GameLog::read_all(&std::fs::read(&path).expect("cannot read the saved games")) {
            let mut s = e.new_match(g.seed);
            writeln!(out, "G {} {} {} self self {}", index, g.seed, m, match g.z { 1 => 0, -1 => 1, _ => -1 }).unwrap();
            for t in &g.ticks {
                if s.result.is_some() {
                    break;
                }
                let f = &s.fighters;
                let see = can_see(&e.rules, &e.map, f[0].pos, f[1].pos) as u8;
                let power = |x: usize| if f[x].power_until == NO_BUFF { 0 } else { (f[x].power_until - s.tick + 1).max(0) };
                writeln!(out, "{} {} {} {} {} {} {} {} {} {}", s.tick, f[0].pos[0], f[0].pos[1], f[1].pos[0], f[1].pos[1], see, f[0].hp.max(0), f[1].hp.max(0), power(0), power(1)).unwrap();
                e.step(&mut s, &t.turns);
            }
            index += 1;
        }
        return;
    }
    let list = arg(&args, "--list").expect("--list FILES.txt or --az FILE.az --map MAP");
    for f in std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let Ok(r) = parse_replay(&text) else { continue };
        if let Some(m) = &map {
            if &r.engine.map.name != m {
                continue;
            }
        }
        if r.bots.len() < 2 {
            continue;
        }
        let e = &r.engine;
        let mut s = e.new_match(r.seed);
        let mut rows: Vec<String> = Vec::new();
        for t in &r.ticks {
            if s.result.is_some() {
                break;
            }
            let f = &s.fighters;
            let see = can_see(&e.rules, &e.map, f[0].pos, f[1].pos) as u8;
            let power = |x: usize| if f[x].power_until == NO_BUFF { 0 } else { (f[x].power_until - s.tick + 1).max(0) };
            rows.push(format!("{} {} {} {} {} {} {} {} {} {}", s.tick, f[0].pos[0], f[0].pos[1], f[1].pos[0], f[1].pos[1], see, f[0].hp.max(0), f[1].hp.max(0), power(0), power(1)));
            e.step(&mut s, &t.turns);
        }
        let f = &s.fighters;
        let winner = if f[0].hp.max(0) > f[1].hp.max(0) { 0 } else if f[1].hp.max(0) > f[0].hp.max(0) { 1 } else { -1 };
        writeln!(out, "G {} {} {} {} {} {}", index, r.seed, e.map.name, r.bots[0].replace(' ', "_"), r.bots[1].replace(' ', "_"), winner).unwrap();
        for row in rows {
            writeln!(out, "{}", row).unwrap();
        }
        index += 1;
    }
}
