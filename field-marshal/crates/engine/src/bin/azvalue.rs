//! azvalue: is a network's value right about real games, by kind of situation?
//!
//!   azvalue --net NET.json --list FILES.txt     (ladder replays, one .jsonl a line)
//!   azvalue --net NET.json --az FILE.az         (saved games: azgen's self-play, azreal's)
//!   azvalue --net NET.json --list FILES.txt --who NAME   (only fighters X that face the bot NAME,
//!                                                 or whose name starts with it when it ends in *)
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

const KINDS: [&str; 24] = [
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
    "X powered, other not, other beyond 8 tiles or unseen",
    "X powered, other not, other within 4 in sight",
    "tick 380 or later (the small last zone)",
    "tick 380 or later and hp within 10",
    "X on a power pad that appears within 5 ticks",
    "the same and the other within 2 tiles of that pad",
    "X within 2 of a power pad appearing within 5 ticks, other not",
    "other ready, X within 4 and by a wall or the map edge",
    "the same and X can step this tick",
    "X slowed (inside the other's slow area)",
    "X slowed, the other's shot ready, within 6 in sight",
    "X slowed and the other within 6 in sight, its shot not ready",
];

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let net = PvNet::load(std::path::Path::new(&arg(&args, "--net").expect("--net NET.json"))).expect("cannot read the network");
    let e = Engine::shipped(&net.map);
    let burst = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Burst { .. }));
    let shot = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. }));
    // the games as a seed and the turns played
    // --who: only positions of the fighter that faces this bot (replays only: saved games carry no names)
    let who = arg(&args, "--who");
    let named = |name: &str| match &who {
        Some(w) => match w.strip_suffix('*') {
            Some(start) => name.to_lowercase().starts_with(&start.to_lowercase()),
            None => name == w,
        },
        None => true,
    };
    // per game: which fighters are the bot --who names (both when not given)
    let mut faced: Vec<[bool; 2]> = Vec::new();
    let mut games: Vec<(i64, Vec<[RawTurn; 2]>)> = Vec::new();
    if let Some(path) = arg(&args, "--az") {
        for g in GameLog::read_all(&std::fs::read(&path).expect("cannot read the saved games")) {
            games.push((g.seed, g.ticks.iter().map(|t| t.turns).collect()));
            faced.push([who.is_none(); 2]);
        }
    } else {
        let list = arg(&args, "--list").expect("--list FILES.txt or --az FILE.az");
        for f in std::fs::read_to_string(&list).expect("cannot read the list").lines().map(str::trim).filter(|l| !l.is_empty()) {
            let Ok(text) = std::fs::read_to_string(f) else { continue };
            let Ok(r) = parse_replay(&text) else { continue };
            if r.engine.map.name != e.map.name || r.engine.rules.hash != e.rules.hash {
                continue;
            }
            let is = [r.bots.first().map(|b| named(b)).unwrap_or(false), r.bots.get(1).map(|b| named(b)).unwrap_or(false)];
            if !is[0] && !is[1] {
                continue;
            }
            games.push((r.seed, r.ticks.iter().map(|t| t.turns).collect()));
            faced.push(is);
        }
    }
    let (mut n, mut value, mut real) = ([0u64; 24], [0.0f64; 24], [0.0f64; 24]);
    // per kind: the positions where the other fighter can act, and where its turn is a dash with the repulsor
    let (mut could, mut slams) = ([0u64; 24], [0u64; 24]);
    // over every position: summed value loss and positions; right about the winner and positions of won games
    let (mut loss, mut positions, mut hit, mut decided) = (0.0f64, 0u64, 0u64, 0u64);
    // the same four by part of the game: ticks 0-199, 200-379, 380 and later
    let mut part = [[0.0f64; 4]; 3];
    // and again for the positions with the two fighters' hp within 10
    let mut level = [[0.0f64; 4]; 3];
    // per game and kind: summed (real - value) and positions
    let mut per: Vec<([f64; 24], [f64; 24])> = Vec::new();
    for (game, (seed, turns)) in games.iter().enumerate() {
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
        per.push(([0.0; 24], [0.0; 24]));
        let slot = per.len() - 1;
        for (at, p) in seen.iter().enumerate() {
            let v0 = net.eval(&e, p).v0 as f64;
            let (chance, target) = (0.5 * (1.0 + v0), 0.5 * (1.0 + z0 as f64));
            loss -= target * (chance + 1e-7).ln() + (1.0 - target) * (1.0 - chance + 1e-7).ln();
            positions += 1;
            let b = if p.tick < 200 { 0 } else if p.tick < 380 { 1 } else { 2 };
            part[b][0] -= target * (chance + 1e-7).ln() + (1.0 - target) * (1.0 - chance + 1e-7).ln();
            part[b][1] += 1.0;
            let close = (p.fighters[0].hp - p.fighters[1].hp).abs() <= 10;
            if close {
                level[b][0] -= target * (chance + 1e-7).ln() + (1.0 - target) * (1.0 - chance + 1e-7).ln();
                level[b][1] += 1.0;
            }
            if z0 != 0.0 {
                decided += 1;
                hit += ((v0 > 0.0) == (z0 > 0.0)) as u64;
                part[b][2] += ((v0 > 0.0) == (z0 > 0.0)) as u64 as f64;
                part[b][3] += 1.0;
                if close {
                    level[b][2] += ((v0 > 0.0) == (z0 > 0.0)) as u64 as f64;
                    level[b][3] += 1.0;
                }
            }
            for x in 0..2 {
                let (me, op) = (&p.fighters[x], &p.fighters[1 - x]);
                if !(me.alive && op.alive) || !faced[game][1 - x] {
                    continue;
                }
                let theirs = &turns[at][1 - x];
                let slam = theirs.dash != victris_engine::turn::DirIn::None && matches!(theirs.attack, victris_engine::turn::AttackIn::Named { attack, .. } if Some(attack as usize) == burst);
                let (v, z) = if x == 0 { (v0, z0 as f64) } else { (-v0, -(z0 as f64)) };
                let near = chebyshev(me.pos, op.pos) <= 4 && can_see(&e.rules, &e.map, me.pos, op.pos);
                let ready = |f: &Fighter| f.state == FState::Idle && f.cd_dash == 0 && burst.map(|b| f.cd_attack[b] == 0).unwrap_or(false);
                let powered = |f: &Fighter| f.power_until > p.tick;
                let by_wall = (-1..=1).any(|dx| (-1..=1).any(|dy| (dx != 0 || dy != 0) && e.map.is_wall([me.pos[0] + dx, me.pos[1] + dy])));
                let threat = near && ready(op);
                // a wall or the edge of the map next to X: what a push can slam it into
                let by_edge = (-1..=1).any(|dx| (-1..=1).any(|dy| {
                    let q = [me.pos[0] + dx, me.pos[1] + dy];
                    (dx != 0 || dy != 0) && (q[0] < 0 || q[1] < 0 || q[0] >= e.map.w || q[1] >= e.map.h || e.map.is_wall(q))
                }));
                // a power pad that is not out yet and appears within 5 ticks: how far each fighter is from the nearest to X
                let soon = e.map.pads.iter().enumerate().filter(|(i, pd)| pd.kind == victris_engine::map::PadKind::Power && !p.pads[*i].available && p.pads[*i].respawn_tick - p.tick <= 5).map(|(_, pd)| (chebyshev(pd.pos, me.pos), chebyshev(pd.pos, op.pos))).min();
                let slowed = victris_engine::observe::slow_ticks_left(&e.map, p, x) > 0;
                let in6 = chebyshev(me.pos, op.pos) <= 6 && can_see(&e.rules, &e.map, me.pos, op.pos);
                let shot_ready = op.state == FState::Idle && shot.map(|b| op.cd_attack[b] == 0).unwrap_or(false);
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
                    powered(me) && !powered(op) && (chebyshev(me.pos, op.pos) > 8 || !can_see(&e.rules, &e.map, me.pos, op.pos)),
                    powered(me) && !powered(op) && near,
                    p.tick >= 380,
                    p.tick >= 380 && (me.hp - op.hp).abs() <= 10,
                    matches!(soon, Some((0, _))),
                    matches!(soon, Some((0, d)) if d <= 2),
                    matches!(soon, Some((a, b)) if a <= 2 && b > 2),
                    threat && by_edge,
                    threat && by_edge && me.cd_move == 0 && me.state == FState::Idle,
                    slowed,
                    slowed && in6 && shot_ready,
                    slowed && in6 && !shot_ready,
                ];
                for k in 0..24 {
                    if kinds[k] {
                        n[k] += 1;
                        value[k] += v;
                        real[k] += z;
                        per[slot].0[k] += z - v;
                        per[slot].1[k] += 1.0;
                        could[k] += (op.state == FState::Idle) as u64;
                        slams[k] += slam as u64;
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
    for (b, name) in ["ticks 0-199", "ticks 200-379", "tick 380 and later"].iter().enumerate() {
        println!(
            "  {:<20} {:>8} positions: value loss {:.4}, picks the winner {:.1}%; with the hp within 10: {} positions, value loss {:.4}, picks the winner {:.1}%",
            name,
            part[b][1] as u64,
            part[b][0] / part[b][1].max(1.0),
            100.0 * part[b][2] / part[b][3].max(1.0),
            level[b][1] as u64,
            level[b][0] / level[b][1].max(1.0),
            100.0 * level[b][2] / level[b][3].max(1.0)
        );
    }
    println!("For fighter X: the network's value, the real result, and the gap (real less value; negative: the network thinks too well of it for X)");
    for k in 0..24 {
        let c = n[k].max(1) as f64;
        let gap = (real[k] - value[k]) / c;
        let err = per.iter().map(|(a, m)| (a[k] - gap * m[k]) * (a[k] - gap * m[k])).sum::<f64>().sqrt() / c;
        println!(
            "  {:<54} {:>8} positions   value {:+.3}   real {:+.3}   gap {:+.3} +-{:.3}   the other dashes with the repulsor {:.1}%",
            KINDS[k],
            n[k],
            value[k] / c,
            real[k] / c,
            gap,
            err,
            100.0 * slams[k] as f64 / could[k].max(1) as f64
        );
    }
}
