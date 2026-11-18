//! oppstats: what a bot does in each tempo situation, measured on ladder replays
//! (workstream 4 of the plan: opponent models).
//!
//!   oppstats --bots A,B,C [--map NAME] [--emit] <replay dir or files>...
//!
//! For every tick on which a named bot could act with its opponent in sight within 6
//! tiles, it records the situation (range, whose shot is ready, whether its own step is
//! ready) and what it did: stepped, dashed or held, and whether it fired, threw a
//! frostburst or used the repulsor. No policy is run, so it is fast; `habits` is the tool
//! that compares the same decisions with what our search would do. With --emit it prints
//! the table as the Rust constant in `crates/bot/src/oppmodel.rs`.

use flate2::read::GzDecoder;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::{add, chebyshev, DIR_VEC};
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::FState;
use victris_engine::turn::{AttackIn, DirIn};

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
    step: u32,
    /// steps that end nearer to the opponent, and farther from it (the rest go sideways)
    toward: u32,
    away: u32,
    dash: u32,
    shot: u32,
    frost: u32,
    burst: u32,
    /// ticks on which the dash, the frostburst, and the repulsor (within 2 tiles) were ready
    dash_ready: u32,
    frost_ready: u32,
    burst_ready: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let only_map: Option<String> = opt("--map");
    let emit = args.iter().any(|a| a == "--emit");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--emit" {
            continue;
        }
        if a == "--bots" || a == "--map" {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    let mut table: BTreeMap<String, Cell> = BTreeMap::new();
    let mut games = 0u32;
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
        let kind_of = |a: u8| match e.rules.attacks[a as usize].kind {
            AttackKind::Projectile { .. } => 0,
            AttackKind::Area { .. } => 1,
            AttackKind::Burst { .. } => 2,
        };
        let Some(ranged) = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. })) else { continue };
        let counted: Vec<bool> = (0..2).map(|i| replay.bots.get(i).map(|n| bots.contains(n)).unwrap_or(false)).collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        games += 1;
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            for x in 0..2 {
                if !counted[x] {
                    continue;
                }
                let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
                let d = chebyshev(f.pos, o.pos);
                if f.state != FState::Idle || d > 6 || !can_see(&e.rules, &e.map, f.pos, o.pos) {
                    continue;
                }
                let band = if d <= 2 { "1-2 tiles" } else if d <= 4 { "3-4 tiles" } else { "5-6 tiles" };
                let mine = if f.cd_attack[ranged] == 0 { "own shot ready" } else { "own shot away " };
                let theirs = if o.cd_attack[ranged] == 0 { "their shot ready" } else { "their shot away " };
                let step = if f.cd_move == 0 { "step ready" } else { "step spent" };
                let c = table.entry(format!("{band} | {mine} | {theirs} | {step}")).or_default();
                c.n += 1;
                c.dash_ready += (f.cd_dash == 0) as u32;
                for (i, a) in e.rules.attacks.iter().enumerate() {
                    if f.cd_attack[i] != 0 {
                        continue;
                    }
                    match a.kind {
                        AttackKind::Area { .. } => c.frost_ready += 1,
                        AttackKind::Burst { .. } if d <= 2 => c.burst_ready += 1,
                        _ => {}
                    }
                }
                let turn = t.turns[x];
                if matches!(turn.dash, DirIn::Dir(_)) && f.cd_dash == 0 {
                    c.dash += 1;
                } else if let (DirIn::Dir(dir), 0) = (turn.mv, f.cd_move) {
                    c.step += 1;
                    let after = chebyshev(add(f.pos, DIR_VEC[dir as usize]), o.pos);
                    c.toward += (after < d) as u32;
                    c.away += (after > d) as u32;
                }
                if let AttackIn::Named { attack, .. } = turn.attack {
                    if f.cd_attack[attack as usize] == 0 {
                        match kind_of(attack) {
                            0 => c.shot += 1,
                            1 => c.frost += 1,
                            _ => c.burst += 1,
                        }
                    }
                }
            }
            e.step(&mut s, &t.turns);
        }
    }
    if emit {
        // Order: range band (1-2, 3-4, 5-6), own shot (away, ready), their shot (away, ready),
        // step (ready, spent): the BTreeMap's order, 24 rows.
        println!("// Generated by `oppstats --emit` from {} games of: {}", games, bots.join(", "));
        println!("// [step toward, step away, step sideways, dash | dash ready, shot, frostburst | ready, repulsor | ready within 2]");
        println!("pub const TABLE: [[f32; 7]; 24] = [");
        for (k, c) in &table {
            let n = c.n.max(1) as f32;
            let r = |a: u32, b: u32| if b == 0 { 0.0 } else { a as f32 / b as f32 };
            println!(
                "    [{:.3}, {:.3}, {:.3}, {:.3}, {:.3}, {:.3}, {:.3}], // {}",
                c.toward as f32 / n,
                c.away as f32 / n,
                (c.step - c.toward - c.away) as f32 / n,
                r(c.dash, c.dash_ready),
                c.shot as f32 / n,
                r(c.frost, c.frost_ready),
                r(c.burst, c.burst_ready),
                k
            );
        }
        println!("];");
        return;
    }
    println!("{} over {games} games", bots.join(", "));
    println!("{:<62} {:>6} {:>6} {:>6} {:>6} {:>6} {:>9} {:>7} {:>6}", "situation", "ticks", "step", "dash", "shot", "frost", "repulsor", "toward", "away");
    let pct = |a: u32, b: u32| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
    for (k, c) in &table {
        if c.n < 30 {
            continue;
        }
        println!(
            "{:<62} {:>6} {:>5.0}% {:>5.0}% {:>5.0}% {:>5.0}% {:>8.0}% {:>6.0}% {:>5.0}%",
            k,
            c.n,
            pct(c.step, c.n),
            pct(c.dash, c.n),
            pct(c.shot, c.n),
            pct(c.frost, c.n),
            pct(c.burst, c.n),
            pct(c.toward, c.n),
            pct(c.away, c.n)
        );
    }
}
