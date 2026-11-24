//! azstyle: how a bot plays, counted from ladder replays.
//!
//!   azstyle --list FILES.txt --who NAME [--vs NAME] [--map arena-21] [--per-game]
//!   azstyle --az GAMES.az --map arena-21      (saved self-play games: both fighters count as X)
//!
//! NAME is a bot's name, or the start of one when it ends in * ('Slop*'). Every game of the
//! list in which a fighter of that name plays (against a bot named by --vs, when given) is
//! replayed on the engine with its events, and for that fighter X and for its opponent O the
//! same things are counted: where they stand to each other, what each fires and what lands
//! (by distance, and by whether the target could step), frost, pushes and slams, dashes, pads,
//! damage by source. Printed a game: X's figure, then O's in the same games. Run it for two
//! bots against the same opponents to see what one does that the other does not. Nothing here
//! is used by a bot: it is a way to look.

use std::collections::HashMap;
use victris_engine::events::Event;
use victris_engine::geom::chebyshev;
use victris_engine::map::PadKind;
use victris_engine::observe::can_see;
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::FState;
use victris_engine::azdata::GameLog;
use victris_engine::turn::{AttackIn, DirIn, RawTurn};
use victris_engine::Engine;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn named(want: &Option<String>, name: &str) -> bool {
    match want {
        Some(w) => match w.strip_suffix('*') {
            Some(start) => name.to_lowercase().starts_with(&start.to_lowercase()),
            None => name == w,
        },
        None => true,
    }
}

struct Game {
    engine: Engine,
    seed: i64,
    bots: Vec<String>,
    turns: Vec<[RawTurn; 2]>,
}

struct Tk {
    turns: [RawTurn; 2],
}

/// Sums by label, for X (0) and for O (1), kept in the order the labels were first used.
/// A power pad is out and this fighter holds no power-up, with the other fighter powered ([0]) or not ([1]):
/// ticks, tiles to the nearest such pad (sum), ended the tick nearer it, further from it, took one.
const LYING: [[&str; 5]; 2] = [
    [
        "unpowered, the other powered, a power pad lying: ticks",
        "  unpowered, the other powered, a power pad lying: tiles to it (sum)",
        "  unpowered, the other powered, a power pad lying: ended the tick nearer it",
        "  unpowered, the other powered, a power pad lying: ended the tick further from it",
        "  unpowered, the other powered, a power pad lying: took it",
    ],
    [
        "neither powered, a power pad lying: ticks",
        "  neither powered, a power pad lying: tiles to it (sum)",
        "  neither powered, a power pad lying: ended the tick nearer it",
        "  neither powered, a power pad lying: ended the tick further from it",
        "  neither powered, a power pad lying: took it",
    ],
];

/// The fighters come back into each other's sight: after how long, and who held the power-up alone.
const MEET: [&str; 6] = [
    "meets the other again after 1-10 ticks out of sight, powered alone",
    "meets the other again after 1-10 ticks out of sight, the other powered alone",
    "meets the other again after 1-10 ticks out of sight, neither or both powered",
    "meets the other again after 11+ ticks out of sight, powered alone",
    "meets the other again after 11+ ticks out of sight, the other powered alone",
    "meets the other again after 11+ ticks out of sight, neither or both powered",
];
/// hp gained on the other over the 15 ticks after such a meeting (sum)
const MEET_GAIN: [&str; 6] = [
    "  hp gained on the other in the 15 ticks after: 1-10 ticks, powered alone",
    "  hp gained on the other in the 15 ticks after: 1-10 ticks, the other powered alone",
    "  hp gained on the other in the 15 ticks after: 1-10 ticks, neither or both powered",
    "  hp gained on the other in the 15 ticks after: 11+ ticks, powered alone",
    "  hp gained on the other in the 15 ticks after: 11+ ticks, the other powered alone",
    "  hp gained on the other in the 15 ticks after: 11+ ticks, neither or both powered",
];

#[derive(Default)]
struct Tally {
    order: Vec<&'static str>,
    v: HashMap<&'static str, [f64; 2]>,
}

impl Tally {
    fn add(&mut self, side: usize, key: &'static str, x: f64) {
        if !self.v.contains_key(key) {
            self.order.push(key);
            self.v.insert(key, [0.0; 2]);
        }
        self.v.get_mut(key).unwrap()[side] += x;
    }
    fn get(&self, key: &str) -> [f64; 2] {
        self.v.get(key).copied().unwrap_or([0.0; 2])
    }
}

fn band(d: i32) -> usize {
    match d {
        ..=2 => 0,
        3..=4 => 1,
        5..=6 => 2,
        _ => 3,
    }
}

const IN_SIGHT: [&str; 4] = ["  in sight at 1-2 tiles", "  in sight at 3-4 tiles", "  in sight at 5-6 tiles", "  in sight at 7+ tiles"];
const SHOT_AT: [&str; 4] = ["  shots fired from 1-2 tiles", "  shots fired from 3-4 tiles", "  shots fired from 5-6 tiles", "  shots fired from 7+ tiles or out of sight"];
const HIT_AT: [&str; 4] = ["  shots landed from 1-2 tiles", "  shots landed from 3-4 tiles", "  shots landed from 5-6 tiles", "  shots landed from 7+ tiles or out of sight"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let who = arg(&args, "--who");
    let vs = arg(&args, "--vs");
    // --result W or L: only the games X won, or only the ones it lost
    let only = arg(&args, "--result");
    let map = arg(&args, "--map");
    let per_game = args.iter().any(|a| a == "--per-game");
    // --trace N: the opening of X's first N games in which it took none of the first two power pads, tick by tick
    let trace: usize = arg(&args, "--trace").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut traced = 0usize;
    let mut t = Tally::default();
    // every label once, so the print order does not depend on what happened first
    for k in [
        "games", "won", "lost", "ticks", "hp at the end", "ticks the other is in sight",
    ] {
        t.add(0, k, 0.0);
    }
    let mut names: HashMap<String, u32> = HashMap::new();
    // the games: ladder replays from --list, or saved self-play games from --az (both fighters are then "self")
    let mut games_in: Vec<Game> = Vec::new();
    if let Some(path) = arg(&args, "--az") {
        let m = map.clone().expect("--az needs --map");
        for g in GameLog::read_all(&std::fs::read(&path).expect("cannot read the saved games")) {
            games_in.push(Game { engine: Engine::shipped(&m), seed: g.seed, bots: vec!["self".into(), "self".into()], turns: g.ticks.iter().map(|t| t.turns).collect() });
        }
    } else {
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
            games_in.push(Game { seed: r.seed, bots: r.bots.clone(), turns: r.ticks.iter().map(|t| t.turns).collect(), engine: r.engine });
        }
    }
    for r in &games_in {
        let e = &r.engine;
        let shot = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. }));
        let frost = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Area { .. }));
        let burst = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Burst { .. }));
        // the fighters to count as X: each one named by --who whose opponent is named by --vs
        let xs: Vec<usize> = (0..2).filter(|&x| named(&who, &r.bots[x]) && named(&vs, &r.bots[1 - x])).collect();
        if xs.is_empty() {
            continue;
        }
        let mut s = e.new_match(r.seed);
        let mut events: Vec<Event> = Vec::new();
        // one game's sums, by fighter index; turned to X and O at the end
        let mut g: [Tally; 2] = [Tally::default(), Tally::default()];
        // a shot in flight: its id -> (who fired it, distance band, the target could not step, the target stood slowed, led)
        let mut flying: HashMap<u32, (usize, usize, bool, bool, bool, bool)> = HashMap::new();
        let mut first_power: Option<usize> = None;
        let mut marks = [[None::<i32>; 2]; 5];
        // the opening: power pads taken before tick 60 (the first pair appears together), and where each
        // fighter stood when they appeared
        let mut wave = [0u32; 2];
        // when each pad last appeared, by pad number
        let mut out_since: HashMap<u8, i32> = HashMap::new();
        let mut appeared = false;
        let mut stood: [Option<i32>; 2] = [None; 2];
        let power_pads: Vec<_> = e.map.pads.iter().filter(|p| p.kind == PadKind::Power).map(|p| p.pos).collect();
        // ticks since the fighters last saw each other (0 while in sight)
        let mut unseen = 0u32;
        let mut story: Vec<String> = Vec::new();
        // the fighters meet again after being out of each other's sight: (tick to read at, kind by fighter, hp then)
        let mut meets: Vec<(i32, [usize; 2], [i32; 2])> = Vec::new();
        for turns in &r.turns {
            let tk = Tk { turns: *turns };
            if s.result.is_some() {
                break;
            }
            let before = s;
            let see = can_see(&e.rules, &e.map, before.fighters[0].pos, before.fighters[1].pos);
            let d = chebyshev(before.fighters[0].pos, before.fighters[1].pos);
            // read the meetings of 15 ticks ago: what each fighter gained on the other since
            meets.retain(|(at, kind, hp)| {
                if before.tick < *at {
                    return true;
                }
                for x in 0..2 {
                    let gain = (before.fighters[x].hp.max(0) - hp[x]) - (before.fighters[1 - x].hp.max(0) - hp[1 - x]);
                    g[x].add(0, MEET_GAIN[kind[x]], gain as f64);
                }
                false
            });
            if see && unseen >= 1 && before.fighters[0].alive && before.fighters[1].alive {
                let mut kind = [0usize; 2];
                for x in 0..2 {
                    let (mine, theirs) = (before.fighters[x].power_until >= before.tick, before.fighters[1 - x].power_until >= before.tick);
                    kind[x] = (if unseen <= 10 { 0 } else { 3 }) + if mine && !theirs { 0 } else if theirs && !mine { 1 } else { 2 };
                    g[x].add(0, MEET[kind[x]], 1.0);
                }
                meets.push((before.tick + 15, kind, [before.fighters[0].hp, before.fighters[1].hp]));
            }
            unseen = if see { 0 } else { unseen + 1 };
            // a fighter standing inside a frost area of the other is slowed
            let slowed = |x: usize| before.areas().iter().any(|a| a.owner as usize == 1 - x && chebyshev(a.pos, before.fighters[x].pos) <= a.radius);
            let stuck = |x: usize| before.fighters[x].state != FState::Idle || before.fighters[x].cd_move > 0;
            for x in 0..2 {
                let (me, op) = (&before.fighters[x], &before.fighters[1 - x]);
                if !(me.alive && op.alive) {
                    continue;
                }
                g[x].add(0, "ticks", 1.0);
                if see {
                    g[x].add(0, "ticks the other is in sight", 1.0);
                    g[x].add(0, IN_SIGHT[band(d)], 1.0);
                    let ready = |a: Option<usize>| a.map(|i| me.cd_attack[i] <= 0).unwrap_or(false);
                    if me.cd_dash <= 0 && ready(burst) {
                        g[x].add(0, "in sight: own dash and push both ready", 1.0);
                    }
                    if ready(shot) {
                        g[x].add(0, "in sight: own shot ready", 1.0);
                    }
                    if ready(frost) {
                        g[x].add(0, "in sight: own frost ready", 1.0);
                    }
                }
                if slowed(x) {
                    g[x].add(0, "ticks slowed in the other's frost", 1.0);
                }
                if me.state == FState::Stunned {
                    g[x].add(0, "ticks stunned", 1.0);
                }
                if me.power_until >= before.tick {
                    g[x].add(0, "ticks powered", 1.0);
                }
                if me.state == FState::Idle && me.cd_move <= 0 && tk.turns[x].mv == DirIn::None && tk.turns[x].dash == DirIn::None {
                    g[x].add(0, "ticks free to step and stood still", 1.0);
                }
                // holding the power-up while the other does not: what is made of it, and what the other does about it
                if me.power_until >= before.tick && op.power_until < before.tick {
                    g[x].add(0, "powered alone: ticks", 1.0);
                    g[x].add(0, "  powered alone: distance to the other (sum)", d as f64);
                    if see {
                        g[x].add(0, "  powered alone: the other in sight", 1.0);
                        if d <= 4 {
                            g[x].add(0, "  powered alone: the other in sight within 4 tiles", 1.0);
                        }
                    }
                    if me.state == FState::Idle && me.cd_move <= 0 && tk.turns[x].mv == DirIn::None && tk.turns[x].dash == DirIn::None {
                        g[x].add(0, "  powered alone: stood still when free to step", 1.0);
                    }
                    // how long the other has been out of sight: the live bot's learned search decides
                    // for the first ticks of that only
                    if !see {
                        g[x].add(0, if unseen <= 10 { "  powered alone: the other out of sight 1-10 ticks" } else { "  powered alone: the other out of sight 11+ ticks" }, 1.0);
                        g[x].add(0, if unseen <= 10 { "  powered alone: tiles to the other, out of sight 1-10 ticks (sum)" } else { "  powered alone: tiles to the other, out of sight 11+ ticks (sum)" }, d as f64);
                    }
                }
                if op.power_until >= before.tick && me.power_until < before.tick {
                    g[x].add(0, "the other powered alone: ticks", 1.0);
                }
                if me.state == FState::Idle && tk.turns[x].attack == AttackIn::None && tk.turns[x].mv == DirIn::None && tk.turns[x].dash == DirIn::None {
                    g[x].add(0, "ticks free and sent nothing", 1.0);
                }
            }
            e.step_events(&mut s, &tk.turns, &mut events);
            if trace > 0 && (8..=70).contains(&before.tick) && before.tick % 2 == 0 {
                let f = &before.fighters;
                let pads: String = e.map.pads.iter().enumerate().filter(|(_, d)| d.kind == PadKind::Power).map(|(i, _)| if before.pads[i].available { 'o' } else { '.' }).collect();
                let pw = |x: usize| if f[x].power_until >= before.tick { "P" } else { " " };
                let st = |x: usize| if f[x].state == FState::Stunned { "S" } else { " " };
                story.push(format!(
                    "  t{:>3} pads {} | 0 at {:>2},{:>2} hp {:>3}{}{} {:?}{:?}{:?} | 1 at {:>2},{:>2} hp {:>3}{}{} {:?}{:?}{:?} | {} apart{}",
                    before.tick, pads, f[0].pos[0], f[0].pos[1], f[0].hp, pw(0), st(0), tk.turns[0].mv, tk.turns[0].dash, tk.turns[0].attack,
                    f[1].pos[0], f[1].pos[1], f[1].hp, pw(1), st(1), tk.turns[1].mv, tk.turns[1].dash, tk.turns[1].attack, d, if see { "" } else { ", out of sight" }
                ));
            }
            let opening = (12..=21).contains(&before.tick);
            // a power pad lies there for the taking and this fighter holds no power-up: does it go for it
            let lying: Vec<[i32; 2]> = e.map.pads.iter().enumerate().filter(|(i, d)| d.kind == PadKind::Power && before.pads[*i].available).map(|(_, d)| d.pos).collect();
            if !lying.is_empty() {
                for x in 0..2 {
                    let (me, op) = (&before.fighters[x], &before.fighters[1 - x]);
                    if !(me.alive && op.alive) || me.power_until >= before.tick {
                        continue;
                    }
                    let k = if op.power_until >= before.tick { 0 } else { 1 };
                    let was = lying.iter().map(|p| chebyshev(*p, me.pos)).min().unwrap_or(0);
                    let now = lying.iter().map(|p| chebyshev(*p, s.fighters[x].pos)).min().unwrap_or(0);
                    g[x].add(0, LYING[k][0], 1.0);
                    g[x].add(0, LYING[k][1], was as f64);
                    if now < was {
                        g[x].add(0, LYING[k][2], 1.0);
                    } else if now > was {
                        g[x].add(0, LYING[k][3], 1.0);
                    }
                    if s.fighters[x].power_until >= s.tick {
                        g[x].add(0, LYING[k][4], 1.0);
                    }
                }
            }
            // one powered alone: does each fighter end the tick nearer to where the other stood, or further
            for x in 0..2 {
                let (me, op) = (&before.fighters[x], &before.fighters[1 - x]);
                if !(me.alive && op.alive) {
                    continue;
                }
                let (mine, theirs) = (me.power_until >= before.tick, op.power_until >= before.tick);
                if mine == theirs {
                    continue;
                }
                let now = chebyshev(s.fighters[x].pos, op.pos);
                if mine && !see && now != d {
                    let k = match (unseen <= 10, now < d) {
                        (true, true) => "  powered alone: ended the tick nearer, out of sight 1-10 ticks",
                        (true, false) => "  powered alone: ended the tick further, out of sight 1-10 ticks",
                        (false, true) => "  powered alone: ended the tick nearer, out of sight 11+ ticks",
                        (false, false) => "  powered alone: ended the tick further, out of sight 11+ ticks",
                    };
                    g[x].add(0, k, 1.0);
                }
                if now < d {
                    g[x].add(0, if mine { "  powered alone: ended the tick nearer the other" } else { "  the other powered alone: ended the tick nearer it" }, 1.0);
                } else if now > d {
                    g[x].add(0, if mine { "  powered alone: ended the tick further from the other" } else { "  the other powered alone: ended the tick further from it" }, 1.0);
                }
            }
            // every pad that appears this tick (the first pair and every return): who stands where
            for ev in &events {
                if let Event::PickupSpawn { pad, kind, pos } = ev {
                    out_since.insert(*pad, s.tick);
                    let power = *kind == PadKind::Power;
                    let dist = [chebyshev(*pos, s.fighters[0].pos), chebyshev(*pos, s.fighters[1].pos)];
                    for x in 0..2 {
                        g[x].add(0, if power { "power pad appears" } else { "health pad appears" }, 1.0);
                        if dist[x] < dist[1 - x] {
                            g[x].add(0, if power { "  power pad appears: nearer to it than the other" } else { "  health pad appears: nearer to it than the other" }, 1.0);
                        }
                        if dist[x] <= 1 {
                            g[x].add(0, if power { "  power pad appears: on it or next to it" } else { "  health pad appears: on it or next to it" }, 1.0);
                        }
                    }
                }
            }
            for x in 0..2 {
                let me = &before.fighters[x];
                if me.alive && before.fighters[1 - x].alive {
                    let near = e.map.pads.iter().map(|p| chebyshev(p.pos, me.pos)).min().unwrap_or(99);
                    if near <= 1 {
                        g[x].add(0, "ticks on or next to a pad's tile", 1.0);
                    }
                    if (me.pos[0] - e.map.w / 2).abs() <= 1 {
                        g[x].add(0, "ticks within 1 column of the middle column", 1.0);
                    }
                }
            }
            for ev in &events {
                match ev {
                    Event::PickupSpawn { kind: PadKind::Power, .. } if !appeared => {
                        appeared = true;
                        for x in 0..2 {
                            let near = power_pads.iter().map(|p| chebyshev(*p, s.fighters[x].pos)).min().unwrap_or(99);
                            stood[x] = Some(near);
                            g[x].add(0, "first power pads appear: distance to the nearer one (sum)", near as f64);
                            if near == 0 {
                                g[x].add(0, "first power pads appear: stands on one", 1.0);
                            }
                            if near <= 1 {
                                g[x].add(0, "first power pads appear: within 1 tile of one", 1.0);
                            }
                            // between the two: within 2 of both
                            if power_pads.iter().all(|p| chebyshev(*p, s.fighters[x].pos) <= 2) {
                                g[x].add(0, "first power pads appear: within 2 tiles of both", 1.0);
                            }
                        }
                    }
                    Event::Attack { fighter, attack, target } => {
                        let x = *fighter as usize - 1;
                        let op = &before.fighters[1 - x];
                        if opening {
                            let a = Some(*attack as usize);
                            g[x].add(0, if a == shot { "ticks 12-21: shots fired" } else if a == frost { "ticks 12-21: frost cast" } else { "ticks 12-21: pushes used" }, 1.0);
                        }
                        if before.fighters[x].power_until >= before.tick && op.power_until < before.tick {
                            let a = Some(*attack as usize);
                            g[x].add(0, if a == shot { "  powered alone: shots fired" } else if a == frost { "  powered alone: frost cast" } else { "  powered alone: pushes used" }, 1.0);
                        }
                        if Some(*attack as usize) == shot {
                            g[x].add(0, "shots fired", 1.0);
                            let b = if see { band(d) } else { 3 };
                            g[x].add(0, SHOT_AT[b], 1.0);
                            if stuck(1 - x) {
                                g[x].add(0, "  shots fired at a target that could not step", 1.0);
                            }
                            if slowed(1 - x) {
                                g[x].add(0, "  shots fired at a target slowed in own frost", 1.0);
                            }
                            if *target != op.pos {
                                g[x].add(0, "  shots aimed off the target's tile", 1.0);
                            }
                            if !see {
                                g[x].add(0, "  shots fired with the other out of sight", 1.0);
                            }
                        } else if Some(*attack as usize) == frost {
                            g[x].add(0, "frost cast", 1.0);
                            let rad = match e.rules.attacks[*attack as usize].kind {
                                AttackKind::Area { radius, .. } => radius,
                                _ => 1,
                            };
                            if chebyshev(*target, op.pos) <= rad {
                                g[x].add(0, "  frost cast over the other's tile", 1.0);
                            }
                            if see {
                                g[x].add(0, "  frost cast with the other in sight", 1.0);
                                g[x].add(0, "  distance when frost was cast (sum)", d as f64);
                            }
                        } else if Some(*attack as usize) == burst {
                            g[x].add(0, "pushes used", 1.0);
                            if tk.turns[x].dash != DirIn::None {
                                g[x].add(0, "  pushes with a dash the same tick", 1.0);
                            }
                            if !see || d > 4 {
                                g[x].add(0, "  pushes with the other beyond 4 tiles or unseen", 1.0);
                            }
                        }
                    }
                    Event::ProjectileSpawn { projectile, owner, target, .. } => {
                        let x = *owner as usize - 1;
                        let b = if see { band(d) } else { 3 };
                        let alone = before.fighters[x].power_until >= before.tick && before.fighters[1 - x].power_until < before.tick;
                        flying.insert(*projectile, (x, b, stuck(1 - x), slowed(1 - x), *target != before.fighters[1 - x].pos, alone));
                    }
                    Event::ProjectileEnd { projectile, reason, .. } => {
                        if let Some((x, b, st, sl, led, alone)) = flying.remove(projectile) {
                            if *reason == "hit" {
                                g[x].add(0, "shots landed", 1.0);
                                if alone {
                                    g[x].add(0, "  powered alone: shots landed", 1.0);
                                }
                                g[x].add(0, HIT_AT[b], 1.0);
                                if st {
                                    g[x].add(0, "  shots landed on a target that could not step when fired", 1.0);
                                }
                                if sl {
                                    g[x].add(0, "  shots landed on a target slowed in own frost when fired", 1.0);
                                }
                                if led {
                                    g[x].add(0, "  shots landed that were aimed off the target's tile", 1.0);
                                }
                            }
                        }
                    }
                    Event::Hit { attacker, target, attack, damage, .. } => {
                        let x = *attacker as usize - 1;
                        if *target == *attacker {
                            g[x].add(0, "damage to itself", *damage as f64);
                            continue;
                        }
                        g[x].add(0, "damage dealt", *damage as f64);
                        if before.fighters[x].power_until >= before.tick {
                            g[x].add(0, "  damage dealt while powered", *damage as f64);
                        }
                        let a = Some(*attack as usize);
                        if a == shot {
                            g[x].add(0, "  damage by shots", *damage as f64);
                        } else if a == frost {
                            g[x].add(0, "  damage by frost blasts", *damage as f64);
                            g[x].add(0, "frost blasts that hit", 1.0);
                        } else if a == burst {
                            g[x].add(0, "  damage by pushes", *damage as f64);
                            g[x].add(0, "  pushes that hit", 1.0);
                        }
                    }
                    Event::WallSlam { attacker, damage, .. } => {
                        let x = *attacker as usize - 1;
                        g[x].add(0, "wall slams", 1.0);
                        g[x].add(0, "damage dealt", *damage as f64);
                        g[x].add(0, "  damage by wall slams", *damage as f64);
                    }
                    Event::Collision { attacker, damage, .. } => {
                        let x = *attacker as usize - 1;
                        g[x].add(0, "  damage by collisions", *damage as f64);
                    }
                    Event::Dash { fighter, from, to } => {
                        let x = *fighter as usize - 1;
                        let op = before.fighters[1 - x].pos;
                        g[x].add(0, "dashes", 1.0);
                        if opening {
                            g[x].add(0, "ticks 12-21: dashes", 1.0);
                        }
                        let (was, now) = (chebyshev(*from, op), chebyshev(*to, op));
                        if now < was {
                            g[x].add(0, "  dashes that close in", 1.0);
                        } else if now > was {
                            g[x].add(0, "  dashes that open up", 1.0);
                        }
                        if slowed(x) {
                            g[x].add(0, "  dashes out of the other's frost", 1.0);
                        }
                    }
                    Event::Pickup { fighter, kind, pad, .. } => {
                        let x = *fighter as usize - 1;
                        if let Some(at) = out_since.remove(pad) {
                            let power = *kind == PadKind::Power;
                            if before.tick - at <= 3 {
                                g[x].add(0, if power { "  power pads taken within 3 ticks of appearing" } else { "  health pads taken within 3 ticks of appearing" }, 1.0);
                            }
                        }
                        match kind {
                            PadKind::Power => {
                                g[x].add(0, "power pads taken", 1.0);
                                if before.tick < 60 {
                                    wave[x] += 1;
                                }
                                let span = ["power pads taken in ticks 0-149", "power pads taken in ticks 150-299", "power pads taken in ticks 300-449", "power pads taken in ticks 450 on"];
                                g[x].add(0, span[(before.tick / 150).clamp(0, 3) as usize], 1.0);
                                if first_power.is_none() {
                                    first_power = Some(x);
                                    g[x].add(0, "  the game's first power pad", 1.0);
                                    g[x].add(0, "  tick of the game's first power pad (sum, own)", before.tick as f64);
                                }
                            }
                            PadKind::Health => {
                                g[x].add(0, "health pads taken", 1.0);
                                g[x].add(0, "  hp gained from health pads", (s.fighters[x].hp - before.fighters[x].hp).max(0) as f64);
                            }
                        }
                    }
                    Event::ZoneDamage { fighter, damage } => {
                        g[*fighter as usize - 1].add(0, "damage taken from the zone", *damage as f64);
                    }
                    _ => {}
                }
            }
            if opening {
                for x in 0..2 {
                    if events.iter().any(|ev| matches!(ev, Event::WallSlam { target, .. } if *target as usize - 1 == x)) {
                        g[x].add(0, "ticks 12-21: slammed into a wall", 1.0);
                    }
                    if events.iter().any(|ev| matches!(ev, Event::Knockback { target, .. } if *target as usize - 1 == x)) {
                        g[x].add(0, "ticks 12-21: pushed away", 1.0);
                    }
                }
            }
            for (k, at) in [40, 100, 200, 300, 400].iter().enumerate() {
                if s.tick == *at && s.result.is_none() {
                    for x in 0..2 {
                        marks[k][x] = Some(s.fighters[x].hp - s.fighters[1 - x].hp);
                    }
                }
            }
        }
        // meetings the game ended on: read at its last position
        for (_, kind, hp) in meets.drain(..) {
            for x in 0..2 {
                let gain = (s.fighters[x].hp.max(0) - hp[x]) - (s.fighters[1 - x].hp.max(0) - hp[1 - x]);
                g[x].add(0, MEET_GAIN[kind[x]], gain as f64);
            }
        }
        let Some(res) = s.result else { continue };
        for &x in &xs {
            if let Some(o) = only.as_deref() {
                let w = res.winner.map(|w| w as usize - 1);
                if (o == "W" && w != Some(x)) || (o == "L" && w != Some(1 - x)) {
                    continue;
                }
            }
            *names.entry(r.bots[x].clone()).or_default() += 1;
            t.add(0, "games", 1.0);
            let won = res.winner.map(|w| w as usize - 1);
            for side in 0..2 {
                let f = if side == 0 { x } else { 1 - x };
                if won == Some(f) {
                    t.add(side, "won", 1.0);
                } else if won.is_some() {
                    t.add(side, "lost", 1.0);
                }
                t.add(side, "hp at the end", s.fighters[f].hp.max(0) as f64);
                for k in &g[f].order {
                    t.add(side, k, g[f].get(k)[0]);
                }
                for (k, label) in ["hp lead at tick 40", "hp lead at tick 100", "hp lead at tick 200", "hp lead at tick 300", "hp lead at tick 400"].iter().enumerate() {
                    if let Some(lead) = marks[k][f] {
                        t.add(side, label, lead as f64);
                        if side == 0 {
                            t.add(0, ["games that reach tick 40", "games that reach tick 100", "games that reach tick 200", "games that reach tick 300", "games that reach tick 400"][k], 1.0);
                        }
                    }
                }
                t.add(side, "power pads taken before tick 60", wave[f] as f64);
                t.add(side, ["  none of the first two power pads", "  one of the first two power pads", "  both of the first two power pads"][wave[f].min(2) as usize], 1.0);
                // the result by how the opening went, for X
                if side == 0 {
                    let label = ["games with none of the first two pads", "games with one of the first two pads", "games with both of the first two pads"][wave[f].min(2) as usize];
                    t.add(0, label, 1.0);
                    if won == Some(f) {
                        t.add(1, label, 1.0);
                    }
                }
            }
            if trace > 0 && wave[x] == 0 && traced < trace {
                traced += 1;
                let w = res.winner.map(|w| w as usize - 1);
                println!("trace {} {} (fighter {}) v {} {}: first pads {}-{}, {} ticks", e.map.name, r.bots[x], x, r.bots[1 - x], if w == Some(x) { "W" } else if w.is_some() { "L" } else { "D" }, wave[x], wave[1 - x], s.tick);
                for l in &story {
                    println!("{}", l);
                }
            }
            if per_game {
                let w = res.winner.map(|w| w as usize - 1);
                println!(
                    "game {} {} v {} {} first pads {}-{} stood {:?}/{:?} lead40 {:?} lead100 {:?} end {} hp {}-{} alone {}-{} seen {}-{} hid11 {}-{} pdmg {}-{} dmg {}-{} lead200 {} lead300 {} lead400 {}",
                    e.map.name,
                    r.bots[x],
                    r.bots[1 - x],
                    if w == Some(x) { "W" } else if w.is_some() { "L" } else { "D" },
                    wave[x],
                    wave[1 - x],
                    stood[x].unwrap_or(-1),
                    stood[1 - x].unwrap_or(-1),
                    marks[0][x].unwrap_or(0),
                    marks[1][x].unwrap_or(0),
                    s.tick,
                    s.fighters[x].hp.max(0),
                    s.fighters[1 - x].hp.max(0),
                    g[x].get("powered alone: ticks")[0],
                    g[1 - x].get("powered alone: ticks")[0],
                    g[x].get("  powered alone: the other in sight")[0],
                    g[1 - x].get("  powered alone: the other in sight")[0],
                    g[x].get("  powered alone: the other out of sight 11+ ticks")[0],
                    g[1 - x].get("  powered alone: the other out of sight 11+ ticks")[0],
                    g[x].get("  damage dealt while powered")[0],
                    g[1 - x].get("  damage dealt while powered")[0],
                    g[x].get("damage dealt")[0],
                    g[1 - x].get("damage dealt")[0],
                    marks[2][x].map(|v| v.to_string()).unwrap_or("na".into()),
                    marks[3][x].map(|v| v.to_string()).unwrap_or("na".into()),
                    marks[4][x].map(|v| v.to_string()).unwrap_or("na".into())
                );
            }
            t.add(0, "game length (ticks)", s.tick as f64);
        }
    }
    let games = t.get("games")[0];
    if games == 0.0 {
        println!("azstyle: no game of {:?} against {:?} in the list", who, vs);
        return;
    }
    let mut by: Vec<(String, u32)> = names.into_iter().collect();
    by.sort_by(|a, b| b.1.cmp(&a.1));
    println!(
        "azstyle: X = {} against {}: {} games ({}). A game: X, then the other fighter in the same games",
        who.clone().unwrap_or("anyone".into()),
        vs.clone().unwrap_or("anyone".into()),
        games,
        by.iter().map(|(n, c)| format!("{n} {c}")).collect::<Vec<_>>().join(", ")
    );
    for k in &t.order {
        let v = t.get(k);
        let per = if k.starts_with("hp lead at tick") {
            let n = t.get(&k.replace("hp lead at", "games that reach"))[0].max(1.0);
            [v[0] / n, v[1] / n]
        } else {
            [v[0] / games, v[1] / games]
        };
        println!("  {:<64} {:>9.2} {:>9.2}", k, per[0], per[1]);
    }
    let rate = |label: &str, top: &str, bottom: &str| {
        let (a, b) = (t.get(top), t.get(bottom));
        let p = |i: usize| if b[i] > 0.0 { 100.0 * a[i] / b[i] } else { 0.0 };
        println!("  {:<64} {:>8.1}% {:>8.1}%", label, p(0), p(1));
    };
    println!("rates:");
    for label in ["games with none of the first two pads", "games with one of the first two pads", "games with both of the first two pads"] {
        let v = t.get(label);
        if v[0] > 0.0 {
            println!("  X won {:>5.1}% of its {:.0} {}", 100.0 * v[1] / v[0], v[0], label);
        }
    }
    rate("shots landed of fired", "shots landed", "shots fired");
    for b in 0..4 {
        rate(&HIT_AT[b].replace("  shots landed", "landed of fired,"), HIT_AT[b], SHOT_AT[b]);
    }
    rate("landed of fired at a target that could not step", "  shots landed on a target that could not step when fired", "  shots fired at a target that could not step");
    rate("landed of fired at a target slowed in own frost", "  shots landed on a target slowed in own frost when fired", "  shots fired at a target slowed in own frost");
    rate("landed of those aimed off the target's tile", "  shots landed that were aimed off the target's tile", "  shots aimed off the target's tile");
    rate("frost blasts that hit, of frost cast", "frost blasts that hit", "frost cast");
    rate("wall slams of pushes used", "wall slams", "pushes used");
    rate("pushes that hit, of pushes used", "  pushes that hit", "pushes used");
    rate("the other in sight, of ticks", "ticks the other is in sight", "ticks");
    rate("dash and push both ready, of ticks in sight", "in sight: own dash and push both ready", "ticks the other is in sight");
    rate("shot ready, of ticks in sight", "in sight: own shot ready", "ticks the other is in sight");
    rate("frost ready, of ticks in sight", "in sight: own frost ready", "ticks the other is in sight");
    rate("damage dealt while powered, of damage dealt", "  damage dealt while powered", "damage dealt");
    for k in 0..6 {
        rate(&format!("{} (x100)", MEET_GAIN[k].trim()), MEET_GAIN[k], MEET[k]);
    }
    for k in 0..2 {
        rate(&format!("{} (x100)", LYING[k][1].trim()), LYING[k][1], LYING[k][0]);
        rate(&format!("{}, of those ticks", LYING[k][2].trim()), LYING[k][2], LYING[k][0]);
        rate(&format!("{}, of those ticks", LYING[k][3].trim()), LYING[k][3], LYING[k][0]);
    }
    rate("powered alone: the other in sight, of those ticks", "  powered alone: the other in sight", "powered alone: ticks");
    rate("powered alone: out of sight 1-10 ticks, of those ticks", "  powered alone: the other out of sight 1-10 ticks", "powered alone: ticks");
    rate("powered alone: out of sight 11+ ticks, of those ticks", "  powered alone: the other out of sight 11+ ticks", "powered alone: ticks");
    rate("powered alone, out of sight 1-10 ticks: tiles to the other (x100)", "  powered alone: tiles to the other, out of sight 1-10 ticks (sum)", "  powered alone: the other out of sight 1-10 ticks");
    rate("powered alone, out of sight 11+ ticks: tiles to the other (x100)", "  powered alone: tiles to the other, out of sight 11+ ticks (sum)", "  powered alone: the other out of sight 11+ ticks");
    rate("powered alone, out of sight 1-10 ticks: ended the tick nearer", "  powered alone: ended the tick nearer, out of sight 1-10 ticks", "  powered alone: the other out of sight 1-10 ticks");
    rate("powered alone, out of sight 1-10 ticks: ended the tick further", "  powered alone: ended the tick further, out of sight 1-10 ticks", "  powered alone: the other out of sight 1-10 ticks");
    rate("powered alone, out of sight 11+ ticks: ended the tick nearer", "  powered alone: ended the tick nearer, out of sight 11+ ticks", "  powered alone: the other out of sight 11+ ticks");
    rate("powered alone, out of sight 11+ ticks: ended the tick further", "  powered alone: ended the tick further, out of sight 11+ ticks", "  powered alone: the other out of sight 11+ ticks");
    rate("powered alone: in sight within 4 tiles, of those ticks", "  powered alone: the other in sight within 4 tiles", "powered alone: ticks");
    rate("powered alone: ended the tick nearer the other, of those ticks", "  powered alone: ended the tick nearer the other", "powered alone: ticks");
    rate("powered alone: ended the tick further away, of those ticks", "  powered alone: ended the tick further from the other", "powered alone: ticks");
    rate("powered alone: stood still when free to step, of those ticks", "  powered alone: stood still when free to step", "powered alone: ticks");
    rate("powered alone: shots fired a 100 ticks", "  powered alone: shots fired", "powered alone: ticks");
    rate("powered alone: shots landed a 100 ticks", "  powered alone: shots landed", "powered alone: ticks");
    rate("powered alone: shots landed of fired", "  powered alone: shots landed", "  powered alone: shots fired");
    rate("powered alone: frost cast a 100 ticks", "  powered alone: frost cast", "powered alone: ticks");
    rate("powered alone: pushes used a 100 ticks", "  powered alone: pushes used", "powered alone: ticks");
    rate("powered alone: tiles to the other (x100)", "  powered alone: distance to the other (sum)", "powered alone: ticks");
    rate("the other powered alone: ended the tick further from it", "  the other powered alone: ended the tick further from it", "the other powered alone: ticks");
    rate("the other powered alone: ended the tick nearer it", "  the other powered alone: ended the tick nearer it", "the other powered alone: ticks");
    rate("power pad appears: nearer to it than the other", "  power pad appears: nearer to it than the other", "power pad appears");
    rate("power pad appears: on it or next to it", "  power pad appears: on it or next to it", "power pad appears");
    rate("health pad appears: nearer to it than the other", "  health pad appears: nearer to it than the other", "health pad appears");
    rate("health pad appears: on it or next to it", "  health pad appears: on it or next to it", "health pad appears");
    rate("on or next to a pad's tile, of ticks", "ticks on or next to a pad's tile", "ticks");
    rate("within 1 column of the middle column, of ticks", "ticks within 1 column of the middle column", "ticks");
    rate("stood still when free to step, of ticks", "ticks free to step and stood still", "ticks");
}
