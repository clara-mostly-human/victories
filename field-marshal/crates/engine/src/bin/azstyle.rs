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
    let map = arg(&args, "--map");
    let per_game = args.iter().any(|a| a == "--per-game");
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
        let mut flying: HashMap<u32, (usize, usize, bool, bool, bool)> = HashMap::new();
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
        for turns in &r.turns {
            let tk = Tk { turns: *turns };
            if s.result.is_some() {
                break;
            }
            let before = s;
            let see = can_see(&e.rules, &e.map, before.fighters[0].pos, before.fighters[1].pos);
            let d = chebyshev(before.fighters[0].pos, before.fighters[1].pos);
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
                if me.state == FState::Idle && tk.turns[x].attack == AttackIn::None && tk.turns[x].mv == DirIn::None && tk.turns[x].dash == DirIn::None {
                    g[x].add(0, "ticks free and sent nothing", 1.0);
                }
            }
            e.step_events(&mut s, &tk.turns, &mut events);
            let opening = (12..=21).contains(&before.tick);
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
                        flying.insert(*projectile, (x, b, stuck(1 - x), slowed(1 - x), *target != before.fighters[1 - x].pos));
                    }
                    Event::ProjectileEnd { projectile, reason, .. } => {
                        if let Some((x, b, st, sl, led)) = flying.remove(projectile) {
                            if *reason == "hit" {
                                g[x].add(0, "shots landed", 1.0);
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
        let Some(res) = s.result else { continue };
        for &x in &xs {
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
            if per_game {
                let w = res.winner.map(|w| w as usize - 1);
                println!(
                    "game {} {} v {} {} first pads {}-{} stood {:?}/{:?} lead40 {:?} lead100 {:?} end {}",
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
                    s.tick
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
    rate("power pad appears: nearer to it than the other", "  power pad appears: nearer to it than the other", "power pad appears");
    rate("power pad appears: on it or next to it", "  power pad appears: on it or next to it", "power pad appears");
    rate("health pad appears: nearer to it than the other", "  health pad appears: nearer to it than the other", "health pad appears");
    rate("health pad appears: on it or next to it", "  health pad appears: on it or next to it", "health pad appears");
    rate("on or next to a pad's tile, of ticks", "ticks on or next to a pad's tile", "ticks");
    rate("within 1 column of the middle column, of ticks", "ticks within 1 column of the middle column", "ticks");
    rate("stood still when free to step, of ticks", "ticks free to step and stood still", "ticks");
}
