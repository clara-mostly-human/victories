//! slamdodge: how exposed each side stands to a wall slam, and whether it could have stepped out.
//!
//!   slamdodge <replay dirs> --bot NAME
//!
//! At every tick where a fighter can act and the opponent could slam it this tick (some turn
//! of the opponent's, dash + repulsor included, slams it given the turn it actually played),
//! the tick counts as "exposed". Reports, for the named bot and for its opponents: exposed
//! ticks a game, how often an exposed tick was punished, and for the slams taken by the named
//! bot, the share of its own turns that would have escaped the opponent's actual turn and
//! what the escape usually was.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::chebyshev;
use victris_engine::mcts::actions_with;
use victris_engine::replay::parse_replay;
use victris_engine::state::FState;
use victris_engine::turn::{DirIn, RawTurn};

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl.gz") || path.to_string_lossy().ends_with(".jsonl") {
        out.push(path.to_path_buf());
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").expect("--bot NAME");
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
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let mut games = 0usize;
    // [us, them]
    let mut decisions = [0usize; 2];
    let mut exposed = [0usize; 2];
    let mut punished = [0usize; 2];
    let mut slams = [0usize; 2];
    let mut exposed_dist = [[0usize; 8]; 2];
    let mut exposed_still = [0usize; 2]; // exposed while holding position (no step, no dash)
    let (mut taken, mut escapable, mut esc_share, mut step_ready) = (0usize, 0usize, 0.0f64, 0usize);
    // ticks where the actor (k = 0: the opponent acting on us; 1: us acting on the opponent) has a
    // turn that slams the other if it holds: how often it plays one, and the move model's mass on them
    let (mut chances, mut took, mut mass) = ([0usize; 2], [0usize; 2], [0.0f64; 2]);
    for path in &files {
        let mut text = String::new();
        if path.to_string_lossy().ends_with(".gz") {
            if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else {
            text = std::fs::read_to_string(path).unwrap_or_default();
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        games += 1;
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            for (k, x) in [me, 1 - me].into_iter().enumerate() {
                let o = 1 - x;
                let f = &s.fighters[x];
                if f.state != FState::Idle || !f.alive || !s.fighters[o].alive {
                    continue;
                }
                decisions[k] += 1;
                let d = chebyshev(f.pos, s.fighters[o].pos) as usize;
                if d > 7 {
                    continue;
                }
                let theirs = actions_with(e, &s, o, 0.0, false, false, false, true);
                let slams_with = |ours: RawTurn, their: RawTurn| {
                    let mut c = s;
                    let mut turns = [RawTurn::IDLE; 2];
                    turns[x] = ours;
                    turns[o] = their;
                    e.step(&mut c, &turns).slammed[x]
                };
                let is_exposed = theirs.iter().any(|a| slams_with(t.turns[x], a.turn));
                {
                    let model = actions_with(e, &s, o, 0.25, false, false, false, true);
                    let slamming: Vec<bool> = model.iter().map(|a| slams_with(RawTurn::IDLE, a.turn)).collect();
                    if slamming.iter().any(|&b| b) {
                        chances[k] += 1;
                        mass[k] += model.iter().zip(&slamming).filter(|(_, &b)| b).map(|(a, _)| a.prior as f64).sum::<f64>();
                        if slams_with(RawTurn::IDLE, t.turns[o]) {
                            took[k] += 1;
                        }
                    }
                }
                let actual = {
                    let mut c = s;
                    e.step(&mut c, &t.turns).slammed[x]
                };
                if actual {
                    slams[k] += 1;
                }
                if is_exposed {
                    exposed[k] += 1;
                    exposed_dist[k][d.min(7)] += 1;
                    let tx = t.turns[x];
                    if matches!(tx.mv, DirIn::None) && matches!(tx.dash, DirIn::None) {
                        exposed_still[k] += 1;
                    }
                    if actual {
                        punished[k] += 1;
                    }
                }
                if actual && k == 0 {
                    taken += 1;
                    step_ready += (f.cd_move == 0) as usize;
                    let ours = actions_with(e, &s, x, 0.0, false, false, false, true);
                    let esc = ours.iter().filter(|a| !slams_with(a.turn, t.turns[o])).count();
                    if esc > 0 {
                        escapable += 1;
                    }
                    esc_share += esc as f64 / ours.len().max(1) as f64;
                }
            }
            e.step(&mut s, &t.turns);
        }
    }
    let g = games.max(1) as f64;
    println!("{games} games of {bot}");
    for (k, who) in ["us", "opponents"].iter().enumerate() {
        println!(
            "  {who}: {:.1} slams taken a game; exposed on {:.1} decision ticks a game ({:.1}% of decisions), {:.0}% of them while holding still; punished {:.1}% of exposed ticks; exposed at distance 1..7: {}",
            slams[k] as f64 / g,
            exposed[k] as f64 / g,
            100.0 * exposed[k] as f64 / decisions[k].max(1) as f64,
            100.0 * exposed_still[k] as f64 / exposed[k].max(1) as f64,
            100.0 * punished[k] as f64 / exposed[k].max(1) as f64,
            (1..8).map(|i| format!("{:.1}", exposed_dist[k][i] as f64 / g)).collect::<Vec<_>>().join(" ")
        );
    }
    for (k, who) in ["opponents slamming us", "us slamming them"].iter().enumerate() {
        println!(
            "  {who}: {:.1} chances a game (a turn that slams if the other holds); took one {:.1}% of them; the move model's mass on those turns {:.1}%",
            chances[k] as f64 / g,
            100.0 * took[k] as f64 / chances[k].max(1) as f64,
            100.0 * mass[k] / chances[k].max(1) as f64
        );
    }
    println!(
        "  slams we took: {taken}; some turn of ours escaped the opponent's actual turn in {:.0}%, on average {:.0}% of our turns escaped; our step was ready in {:.0}%",
        100.0 * escapable as f64 / taken.max(1) as f64,
        100.0 * esc_share / taken.max(1) as f64,
        100.0 * step_ready as f64 / taken.max(1) as f64
    );
}
