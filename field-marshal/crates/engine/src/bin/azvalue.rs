//! azvalue: is a network's value right about real games, by kind of situation?
//!
//!   azvalue --net NET.json --list FILES.txt     (ladder replays, one .jsonl a line)
//!   azvalue --net NET.json --az FILE.az         (saved games: azgen's self-play, azreal's)
//!
//! For every position of every game that ends in a result, and each fighter X: the network's
//! value for X (-1 to 1, no search) beside the game's real result for X, averaged over the
//! positions of one kind. The gap is real less value: a negative gap is a kind of position the
//! network thinks better of, for X, than the games bear out. The error given counts each game
//! once (a game's positions all share its result), so a gap of three errors or more is real.
//! Run on self-play games it shows what self-play taught; run on ladder replays it shows where
//! the ladder disagrees. The kinds are a list to look through, nothing the bot uses.
//!
//! The first line is the whole in two figures, counted as the trainer counts them: the value
//! loss (cross-entropy of the chance to win against the real result) and how often the value
//! names the real winner, over every position of the games that had a winner.

use victris_engine::azdata::GameLog;
use victris_engine::geom::chebyshev;
use victris_engine::observe::can_see;
use victris_engine::pvnet::PvNet;
use victris_engine::pvsearch::outcome;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::{FState, Fighter, State};
use victris_engine::{Engine, RawTurn};

const KINDS: [&str; 12] = [
    "all positions",
    "in sight within 4 tiles",
    "X: dash and repulsor ready, other within 4",
    "other: dash and repulsor ready, X within 4",
    "other ready as above and X next to a wall",
    "other ready, X by a wall, and X cannot step this tick",
    "other ready and powered, X within 4",
    "X powered, other not",
    "other powered, X not",
    "X stunned now",
    "X ahead by 15 hp or more",
    "even hp (within 5), nobody powered",
];

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let net = PvNet::load(std::path::Path::new(&arg(&args, "--net").expect("--net NET.json"))).expect("cannot read the network");
    let e = Engine::shipped(&net.map);
    let burst = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Burst { .. }));
    // the games as a seed and the turns played
    let mut games: Vec<(i64, Vec<[RawTurn; 2]>)> = Vec::new();
    if let Some(path) = arg(&args, "--az") {
        for g in GameLog::read_all(&std::fs::read(&path).expect("cannot read the saved games")) {
            games.push((g.seed, g.ticks.iter().map(|t| t.turns).collect()));
        }
    } else {
        let list = arg(&args, "--list").expect("--list FILES.txt or --az FILE.az");
        for f in std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(text) = std::fs::read_to_string(f) else { continue };
            let Ok(r) = parse_replay(&text) else { continue };
            if r.engine.map.name != e.map.name || r.engine.rules.hash != e.rules.hash {
                continue;
            }
            games.push((r.seed, r.ticks.iter().map(|t| t.turns).collect()));
        }
    }
    let (mut n, mut value, mut real) = ([0u64; 12], [0.0f64; 12], [0.0f64; 12]);
    // over every position: summed value loss and positions; right about the winner and positions of won games
    let (mut loss, mut positions, mut hit, mut decided) = (0.0f64, 0u64, 0u64, 0u64);
    // per game and kind: summed (real - value) and positions
    let mut per: Vec<([f64; 12], [f64; 12])> = Vec::new();
    for (seed, turns) in &games {
        let mut s = e.new_match(*seed);
        let mut seen: Vec<State> = Vec::new();
        for t in turns {
            if s.result.is_some() {
                break;
            }
            seen.push(s);
            e.step(&mut s, t);
        }
        let Some(z0) = outcome(&s) else { continue };
        per.push(([0.0; 12], [0.0; 12]));
        let slot = per.len() - 1;
        for p in &seen {
            let v0 = net.eval(&e, p).v0 as f64;
            let (chance, target) = (0.5 * (1.0 + v0), 0.5 * (1.0 + z0 as f64));
            loss -= target * (chance + 1e-7).ln() + (1.0 - target) * (1.0 - chance + 1e-7).ln();
            positions += 1;
            if z0 != 0.0 {
                decided += 1;
                hit += ((v0 > 0.0) == (z0 > 0.0)) as u64;
            }
            for x in 0..2 {
                let (me, op) = (&p.fighters[x], &p.fighters[1 - x]);
                if !(me.alive && op.alive) {
                    continue;
                }
                let (v, z) = if x == 0 { (v0, z0 as f64) } else { (-v0, -(z0 as f64)) };
                let near = chebyshev(me.pos, op.pos) <= 4 && can_see(&e.rules, &e.map, me.pos, op.pos);
                let ready = |f: &Fighter| f.state == FState::Idle && f.cd_dash == 0 && burst.map(|b| f.cd_attack[b] == 0).unwrap_or(false);
                let powered = |f: &Fighter| f.power_until > p.tick;
                let by_wall = (-1..=1).any(|dx| (-1..=1).any(|dy| (dx != 0 || dy != 0) && e.map.is_wall([me.pos[0] + dx, me.pos[1] + dy])));
                let threat = near && ready(op);
                let kinds = [
                    true,
                    near,
                    near && ready(me),
                    threat,
                    threat && by_wall,
                    threat && by_wall && (me.cd_move > 0 || me.state != FState::Idle),
                    threat && powered(op),
                    powered(me) && !powered(op),
                    powered(op) && !powered(me),
                    me.state == FState::Stunned,
                    me.hp - op.hp >= 15,
                    (me.hp - op.hp).abs() <= 5 && !powered(me) && !powered(op),
                ];
                for k in 0..12 {
                    if kinds[k] {
                        n[k] += 1;
                        value[k] += v;
                        real[k] += z;
                        per[slot].0[k] += z - v;
                        per[slot].1[k] += 1.0;
                    }
                }
            }
        }
    }
    println!(
        "azvalue {}: {} games, {} positions: value loss {:.4}, picks the winner {:.1}%",
        net.map,
        per.len(),
        positions,
        loss / positions.max(1) as f64,
        100.0 * hit as f64 / decided.max(1) as f64
    );
    println!("For fighter X: the network's value, the real result, and the gap (real less value; negative: the network thinks too well of it for X)");
    for k in 0..12 {
        let c = n[k].max(1) as f64;
        let gap = (real[k] - value[k]) / c;
        let err = per.iter().map(|(a, m)| (a[k] - gap * m[k]) * (a[k] - gap * m[k])).sum::<f64>().sqrt() / c;
        println!("  {:<54} {:>8} positions   value {:+.3}   real {:+.3}   gap {:+.3} +-{:.3}", KINDS[k], n[k], value[k] / c, real[k] / c, gap, err);
    }
}
