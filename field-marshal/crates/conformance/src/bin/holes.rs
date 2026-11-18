//! holes: which turns do real bots play that the search cannot even consider?
//!
//!   holes <replay dir or files>... [--bots A,B] [--dash-burst] [--show 20]
//!
//! For every tick on which a fighter could act, checks whether the turn it played is in
//! the search's move list for that position (`mcts::actions_with`, compared on movement,
//! dash and attack; shouts ignored). Turns that are missing are counted by kind: what moved
//! (hold, step, dash), which attack, how far the opponent was, and for aimed attacks how
//! far the aim was from the opponent's tile. A missing turn played by a strong bot is a
//! move our search can neither play nor expect.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::chebyshev;
use victris_engine::mcts::actions_with;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::FState;
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};
use victris_engine::Engine;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else {
        let s = path.to_string_lossy();
        if s.ends_with(".jsonl.gz") || s.ends_with(".jsonl") {
            out.push(path.to_path_buf());
        }
    }
}

fn same(a: &RawTurn, b: &RawTurn) -> bool {
    a.mv == b.mv && a.dash == b.dash && a.attack == b.attack
}

fn kind(e: &Engine, t: &RawTurn, d: i32, opp: [i32; 2], seen: bool) -> String {
    let mv = match (t.mv, t.dash) {
        (_, DirIn::Dir(_)) => "dash",
        (DirIn::Dir(_), _) => "step",
        _ => "hold",
    };
    let atk = match t.attack {
        AttackIn::Named { attack, target } => {
            let a = &e.rules.attacks[attack as usize];
            let name = match a.kind {
                AttackKind::Projectile { .. } => "shot",
                AttackKind::Area { .. } => "frost",
                AttackKind::Burst { .. } => "repulsor",
            };
            match target {
                TargetIn::Tile(p) if !a.is_burst() => format!("{name} aimed {} off", chebyshev(p, opp)),
                _ => name.to_string(),
            }
        }
        AttackIn::None => "no attack".into(),
        _ => "bad attack".into(),
    };
    let band = match d {
        0..=2 => "within 2",
        3 => "at 3",
        4..=5 => "at 4-5",
        6..=8 => "at 6-8",
        _ => "beyond 8",
    };
    format!("{mv} + {atk}, {band}{}", if seen { "" } else { ", out of sight" })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bots: Vec<String> = opt("--bots").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let dash_burst = args.iter().any(|a| a == "--dash-burst");
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bots" || a == "--show" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let (mut games, mut decisions, mut missing, mut missing_attacks) = (0usize, 0usize, 0usize, 0usize);
    let mut by_kind: HashMap<String, usize> = HashMap::new();
    let mut attacking = 0usize;
    for path in &files {
        let name = path.to_string_lossy().to_string();
        let mut text = String::new();
        if name.ends_with(".gz") {
            if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else {
            text = std::fs::read_to_string(path).unwrap_or_default();
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let counted: Vec<bool> = (0..2).map(|i| bots.is_empty() || replay.bots.get(i).map(|n| bots.contains(n)).unwrap_or(false)).collect();
        if !counted[0] && !counted[1] {
            continue;
        }
        games += 1;
        let e = &replay.engine;
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
                if f.state != FState::Idle || !f.alive || !o.alive {
                    continue;
                }
                let played = t.turns[x];
                let attacks = !matches!(played.attack, AttackIn::None);
                decisions += 1;
                attacking += attacks as usize;
                let list = actions_with(e, &s, x, 0.25, false, false, false, dash_burst);
                if list.iter().any(|a| same(&a.turn, &played)) {
                    continue;
                }
                // A turn the engine would not carry out as asked (a step into a wall, an
                // attack on cooldown) is not a hole in the list.
                let blocked = matches!(played.mv, DirIn::Dir(dir) if e.map.step_blocked(f.pos, dir) && !matches!(played.dash, DirIn::Dir(_)));
                let cooling = matches!(played.attack, AttackIn::Named { attack, .. } if f.cd_attack[attack as usize] > 0);
                let step_cd = matches!(played.mv, DirIn::Dir(_)) && f.cd_move > 0 && !matches!(played.dash, DirIn::Dir(_));
                let dash_cd = matches!(played.dash, DirIn::Dir(_)) && f.cd_dash > 0;
                if blocked || cooling || step_cd || dash_cd || matches!(played.mv, DirIn::Bad) || matches!(played.dash, DirIn::Bad) {
                    continue;
                }
                missing += 1;
                missing_attacks += attacks as usize;
                let seen = can_see(&e.rules, &e.map, f.pos, o.pos);
                *by_kind.entry(kind(e, &played, chebyshev(f.pos, o.pos), o.pos, seen)).or_default() += 1;
            }
            e.step(&mut s, &t.turns);
        }
    }
    println!(
        "{games} games, {decisions} decisions that could be carried out; {missing} ({:.1}%) not in the search's move list; of the {attacking} with an attack, {missing_attacks} ({:.1}%) not in it",
        100.0 * missing as f64 / decisions.max(1) as f64,
        100.0 * missing_attacks as f64 / attacking.max(1) as f64
    );
    let mut kinds: Vec<_> = by_kind.into_iter().collect();
    kinds.sort_by(|a, b| b.1.cmp(&a.1));
    for (k, n) in kinds.into_iter().take(show) {
        println!("  {:>6}  {:>5.2}% of decisions  {k}", n, 100.0 * n as f64 / decisions.max(1) as f64);
    }
}
