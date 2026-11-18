//! What one fighter is told each tick, ported from `packages/engine/src/observe.ts`,
//! `options.ts`, `danger.ts` and `sight.ts`.
//!
//! [`View`] is the per-tick observation in typed form. It can be built two ways: from the
//! JSON the server sends ([`View::from_json`]) and from a full state ([`View::from_state`]).
//! Building it from a state is how a bot predicts its next observation, and comparing the
//! two is how the port of the observation rules is checked against the official engine.
//! The event list is not part of a `View`; events are read separately.

use crate::geom::{add, chebyshev, dir_from_name, ShotBuf, DIR_VEC, P};
use crate::map::{MapData, PadKind};
use crate::rules::{CdSlot, Rules, MAX_ATTACKS};
use crate::state::*;
use crate::step::{advance_for_danger, zone_radius_at};
use crate::Engine;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfView {
    pub id: u8,
    pub pos: P,
    pub hp: i32,
    pub max_hp: i32,
    pub state: FState,
    pub state_ticks_left: i32,
    pub slow_ticks_left: i32,
    /// ticks of power buff left, counting the tick being replied to
    pub power_ticks_left: Option<i32>,
    pub cd_move: i32,
    pub cd_dash: i32,
    pub cd_shout: i32,
    pub cd_attack: [i32; MAX_ATTACKS],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnemyView {
    pub id: u8,
    pub pos: P,
    pub hp: i32,
    pub dash_cooldown: i32,
    pub state: FState,
    pub state_ticks_left: i32,
    pub slow_ticks_left: i32,
    pub power_ticks_left: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectileView {
    pub id: u32,
    pub pos: P,
    pub exact_pos: P,
    pub origin: P,
    pub target: P,
    pub speed: i32,
    /// fighter id (1 or 2)
    pub owner: u8,
    pub damage: i32,
    pub attack: u8,
    pub bounces_left: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AreaView {
    pub id: u32,
    pub owner: u8,
    pub attack: u8,
    pub pos: P,
    pub damage: i32,
    pub radius: i32,
    pub slow: i32,
    pub ticks_left: i32,
    pub tiles: Vec<P>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickupView {
    pub id: u8,
    pub pos: P,
    pub kind: PadKind,
    /// 0 = on the pad now
    pub ready_in: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DashOption {
    pub dir: u8,
    pub tile: P,
    pub distance: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Danger {
    pub tile: P,
    pub tick: i32,
    pub attack: u8,
    pub owner: u8,
    pub damage: i32,
    pub from_area: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub tick: i32,
    pub me: SelfView,
    /// the opponent, when in sight
    pub enemy: Option<EnemyView>,
    pub projectiles: Vec<ProjectileView>,
    pub areas: Vec<AreaView>,
    pub pickups: Vec<PickupView>,
    /// (direction, tile reached) for each step that could start now
    pub moves: Vec<(u8, P)>,
    pub dashes: Vec<DashOption>,
    pub zone_radius: i32,
    /// (radius after the next shrink, ticks until it), when within the warning window
    pub next_shrink: Option<(i32, i32)>,
    pub danger: Vec<Danger>,
}

/// Can a fighter standing on `from` see tile `pos`?
#[inline]
pub fn can_see(rules: &Rules, map: &MapData, from: P, pos: P) -> bool {
    chebyshev(from, pos) <= rules.sight_radius && map.has_line_of_sight(from, pos)
}

/// Exact blast footprint: the square, minus walls and tiles a wall shields from the centre.
pub fn blast_tiles(map: &MapData, pos: P, radius: i32) -> Vec<P> {
    let mut out = Vec::new();
    for y in (pos[1] - radius)..=(pos[1] + radius) {
        for x in (pos[0] - radius)..=(pos[0] + radius) {
            if !map.is_wall([x, y]) && map.has_line_of_sight(pos, [x, y]) {
                out.push([x, y]);
            }
        }
    }
    out
}

fn ticks_left(s: &State, f: &Fighter) -> i32 {
    if f.state == FState::Recovering || f.state == FState::Stunned {
        (f.state_end - s.tick + 1).max(0)
    } else {
        0
    }
}

/// Ticks until the slow area that is slowing fighter `i` this tick explodes, or 0.
pub fn slow_ticks_left(map: &MapData, s: &State, i: usize) -> i32 {
    let mut left = 0;
    for b in s.areas() {
        if b.slow == 0 || s.tick <= b.launched_tick || s.tick > b.explodes_tick {
            continue;
        }
        if b.owner as usize != i && map.in_blast(b.pos, b.radius, s.fighters[i].pos) {
            left = left.max(b.explodes_tick - s.tick + 1);
        }
    }
    left
}

fn power_left(s: &State, f: &Fighter) -> Option<i32> {
    if f.power_until == NO_BUFF {
        None
    } else {
        Some(f.power_until - s.tick + 1)
    }
}

/// The next tick after `tick` at which the zone radius changes, with the new radius.
pub fn next_shrink(rules: &Rules, map: &MapData, tick: i32) -> Option<(i32, i32)> {
    let z = &rules.zone;
    let now = zone_radius_at(rules, map, tick);
    if now <= z.min_radius {
        return None;
    }
    let mut t = if tick < z.start_tick {
        z.start_tick
    } else {
        z.start_tick + ((tick - z.start_tick).div_euclid(z.interval) + 1) * z.interval
    };
    // The first scheduled shrink can leave the radius unchanged; skip those.
    for _ in 0..100 {
        let r = zone_radius_at(rules, map, t);
        if r < now {
            return Some((t, r));
        }
        t += z.interval;
    }
    None
}

/// Every step and dash fighter `i` can start now, as the observation lists them.
pub fn move_options(rules: &Rules, map: &MapData, s: &State, i: usize) -> (Vec<(u8, P)>, Vec<DashOption>) {
    let mut moves = Vec::new();
    let mut dashes = Vec::new();
    let f = &s.fighters[i];
    if !f.alive || f.state != FState::Idle {
        return (moves, dashes);
    }
    let o = &s.fighters[1 - i];
    let steps_from = |from: P, dir: u8| -> Option<P> {
        if map.step_blocked(from, dir) {
            return None;
        }
        let to = add(from, DIR_VEC[dir as usize]);
        if o.alive && o.pos == to {
            None
        } else {
            Some(to)
        }
    };
    for dir in 0..8u8 {
        if f.cd_move == 0 {
            if let Some(to) = steps_from(f.pos, dir) {
                moves.push((dir, to));
            }
        }
        if f.cd_dash == 0 {
            let mut p = f.pos;
            let mut distance = 0;
            while distance < rules.dash_distance {
                match steps_from(p, dir) {
                    Some(next) => p = next,
                    None => break,
                }
                distance += 1;
            }
            if distance > 0 {
                dashes.push(DashOption { dir, tile: p, distance });
            }
        }
    }
    (moves, dashes)
}

impl View {
    /// The world as fighter index `me` sees it in state `s` (the state before tick `s.tick`).
    pub fn from_state(e: &Engine, s: &State, me: usize) -> View {
        let (rules, map) = (&e.rules, &e.map);
        let f = &s.fighters[me];
        let o = &s.fighters[1 - me];
        let sees = |p: P| can_see(rules, map, f.pos, p);
        let enemy = if o.alive && sees(o.pos) {
            Some(EnemyView {
                id: (2 - me) as u8,
                pos: o.pos,
                hp: o.hp,
                dash_cooldown: o.cd_dash,
                state: o.state,
                state_ticks_left: ticks_left(s, o),
                slow_ticks_left: slow_ticks_left(map, s, 1 - me),
                power_ticks_left: power_left(s, o),
            })
        } else {
            None
        };
        let seen_projectiles: Vec<&Projectile> =
            s.projectiles().iter().filter(|p| p.owner as usize == me || sees(p.pos)).collect();
        let seen_areas: Vec<&Area> = s.areas().iter().filter(|b| b.owner as usize == me || sees(b.pos)).collect();

        let mut danger = Vec::new();
        let mut buf = ShotBuf::new();
        for p in &seen_projectiles {
            if p.owner as usize == me {
                continue;
            }
            // Fly a copy until it stops, recording each open tile it sweeps and when.
            let mut q = **p;
            let mut t = s.tick;
            loop {
                let flying = advance_for_danger(map, &mut q, &mut buf, |tile| {
                    danger.push(Danger { tile, tick: t, attack: p.attack, owner: p.owner + 1, damage: q_damage(p), from_area: false });
                });
                if !flying {
                    break;
                }
                t += 1;
            }
        }
        for b in &seen_areas {
            if b.owner as usize == me {
                continue;
            }
            for tile in blast_tiles(map, b.pos, b.radius) {
                danger.push(Danger { tile, tick: b.explodes_tick, attack: b.attack, owner: b.owner + 1, damage: b.damage, from_area: true });
            }
        }
        danger.sort_by_key(|d| d.tick); // stable, like the engine's sort

        let (moves, dashes) = move_options(rules, map, s, me);
        // The next shrink is only announced within the warning window.
        let next = next_shrink(rules, map, s.tick - 1)
            .filter(|(t, _)| t - s.tick < rules.zone.warning_ticks)
            .map(|(t, r)| (r, t - s.tick));
        View {
            tick: s.tick,
            me: SelfView {
                id: (me + 1) as u8,
                pos: f.pos,
                hp: f.hp,
                max_hp: rules.max_hp,
                state: f.state,
                state_ticks_left: ticks_left(s, f),
                slow_ticks_left: slow_ticks_left(map, s, me),
                power_ticks_left: power_left(s, f),
                cd_move: f.cd_move,
                cd_dash: f.cd_dash,
                cd_shout: f.cd_shout,
                cd_attack: f.cd_attack,
            },
            enemy,
            projectiles: seen_projectiles
                .iter()
                .map(|p| ProjectileView {
                    id: p.id,
                    pos: p.pos,
                    exact_pos: p.exact_pos,
                    origin: p.origin,
                    target: p.target,
                    speed: p.speed,
                    owner: p.owner + 1,
                    damage: p.damage,
                    attack: p.attack,
                    bounces_left: p.bounces_left,
                })
                .collect(),
            areas: seen_areas
                .iter()
                .map(|b| AreaView {
                    id: b.id,
                    owner: b.owner + 1,
                    attack: b.attack,
                    pos: b.pos,
                    damage: b.damage,
                    radius: b.radius,
                    slow: b.slow,
                    ticks_left: b.explodes_tick - s.tick + 1,
                    tiles: blast_tiles(map, b.pos, b.radius),
                })
                .collect(),
            pickups: map
                .pads
                .iter()
                .enumerate()
                .filter(|(_, d)| sees(d.pos))
                .map(|(i, d)| PickupView {
                    id: (i + 1) as u8,
                    pos: d.pos,
                    kind: d.kind,
                    ready_in: if s.pads[i].available { 0 } else { s.pads[i].respawn_tick - s.tick },
                })
                .collect(),
            moves,
            dashes,
            zone_radius: s.zone_radius,
            next_shrink: next,
            danger,
        }
    }

    /// Reads an `observation` message. Unknown fields are ignored, as the protocol asks.
    pub fn from_json(e: &Engine, v: &Value) -> Result<View, String> {
        let rules = &e.rules;
        let int = |v: &Value, k: &str| -> Result<i32, String> {
            v.get(k).and_then(Value::as_i64).map(|n| n as i32).ok_or_else(|| format!("observation: {k} missing"))
        };
        let tile = |v: &Value, k: &str| -> Result<P, String> { as_tile(v.get(k)).ok_or_else(|| format!("observation: {k} missing")) };
        let state = |v: &Value| -> Result<FState, String> {
            match v.get("state").and_then(Value::as_str) {
                Some("idle") => Ok(FState::Idle),
                Some("dashing") => Ok(FState::Dashing),
                Some("recovering") => Ok(FState::Recovering),
                Some("stunned") => Ok(FState::Stunned),
                Some("dead") => Ok(FState::Dead),
                other => Err(format!("observation: unknown state {other:?}")),
            }
        };
        let power = |v: &Value| v.get("buffs").and_then(|b| b.get("power")).and_then(Value::as_i64).map(|n| n as i32);
        let attack = |v: &Value| -> Result<u8, String> {
            let name = v.get("attack").and_then(Value::as_str).unwrap_or("");
            rules.attack_index(name).ok_or_else(|| format!("observation: unknown attack {name:?}"))
        };
        let list = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]);

        let me_v = v.get("self").ok_or("observation: self missing")?;
        let cds = me_v.get("cooldowns");
        let mut me = SelfView {
            id: int(me_v, "id")? as u8,
            pos: tile(me_v, "pos")?,
            hp: int(me_v, "hp")?,
            max_hp: int(me_v, "max_hp")?,
            state: state(me_v)?,
            state_ticks_left: int(me_v, "state_ticks_left")?,
            slow_ticks_left: int(me_v, "slow_ticks_left")?,
            power_ticks_left: power(me_v),
            cd_move: 0,
            cd_dash: 0,
            cd_shout: 0,
            cd_attack: [0; MAX_ATTACKS],
        };
        for (name, slot) in &rules.cooldown_keys {
            let n = cds.and_then(|c| c.get(name)).and_then(Value::as_i64).unwrap_or(0) as i32;
            match slot {
                CdSlot::Move => me.cd_move = n,
                CdSlot::Dash => me.cd_dash = n,
                CdSlot::Shout => me.cd_shout = n,
                CdSlot::Attack(a) => me.cd_attack[*a as usize] = n,
            }
        }
        let enemy = match list("enemies").first() {
            None => None,
            Some(x) => Some(EnemyView {
                id: int(x, "id")? as u8,
                pos: tile(x, "pos")?,
                hp: int(x, "hp")?,
                dash_cooldown: int(x, "dash_cooldown")?,
                state: state(x)?,
                state_ticks_left: int(x, "state_ticks_left")?,
                slow_ticks_left: int(x, "slow_ticks_left")?,
                power_ticks_left: power(x),
            }),
        };
        let mut projectiles = Vec::new();
        for p in list("projectiles") {
            projectiles.push(ProjectileView {
                id: int(p, "id")? as u32,
                pos: tile(p, "pos")?,
                exact_pos: tile(p, "exact_pos")?,
                origin: tile(p, "origin")?,
                target: tile(p, "target")?,
                speed: int(p, "speed")?,
                owner: int(p, "owner")? as u8,
                damage: int(p, "damage")?,
                attack: attack(p)?,
                bounces_left: int(p, "bounces_left")?,
            });
        }
        let mut areas = Vec::new();
        for b in list("areas") {
            areas.push(AreaView {
                id: int(b, "id")? as u32,
                owner: int(b, "owner")? as u8,
                attack: attack(b)?,
                pos: tile(b, "pos")?,
                damage: int(b, "damage")?,
                radius: int(b, "radius")?,
                slow: int(b, "slow")?,
                ticks_left: int(b, "ticks_left")?,
                tiles: b.get("tiles").and_then(Value::as_array).map(|a| a.iter().filter_map(|t| as_tile(Some(t))).collect()).unwrap_or_default(),
            });
        }
        let mut pickups = Vec::new();
        for p in list("pickups") {
            pickups.push(PickupView {
                id: int(p, "id")? as u8,
                pos: tile(p, "pos")?,
                kind: match p.get("kind").and_then(Value::as_str) {
                    Some("health") => PadKind::Health,
                    Some("power") => PadKind::Power,
                    other => return Err(format!("observation: unknown pickup kind {other:?}")),
                },
                ready_in: int(p, "ready_in")?,
            });
        }
        let dir = |x: &Value| -> Result<u8, String> {
            x.get("dir").and_then(Value::as_str).and_then(dir_from_name).ok_or_else(|| "observation: bad dir".to_string())
        };
        let mut moves = Vec::new();
        for m in list("moves") {
            moves.push((dir(m)?, tile(m, "tile")?));
        }
        let mut dashes = Vec::new();
        for d in list("dashes") {
            dashes.push(DashOption { dir: dir(d)?, tile: tile(d, "tile")?, distance: int(d, "distance")? });
        }
        let mut danger = Vec::new();
        for d in list("danger") {
            danger.push(Danger {
                tile: tile(d, "tile")?,
                tick: int(d, "tick")?,
                attack: attack(d)?,
                owner: int(d, "owner")? as u8,
                damage: int(d, "damage")?,
                from_area: d.get("source").and_then(Value::as_str) == Some("area"),
            });
        }
        let zone = v.get("zone").ok_or("observation: zone missing")?;
        let next_shrink = match (zone.get("next_radius").and_then(Value::as_i64), zone.get("shrinks_in").and_then(Value::as_i64)) {
            (Some(r), Some(t)) => Some((r as i32, t as i32)),
            _ => None,
        };
        Ok(View {
            tick: int(v, "tick")?,
            me,
            enemy,
            projectiles,
            areas,
            pickups,
            moves,
            dashes,
            zone_radius: int(zone, "radius")?,
            next_shrink,
            danger,
        })
    }
}

#[inline]
fn q_damage(p: &Projectile) -> i32 {
    p.damage
}

pub fn as_tile(v: Option<&Value>) -> Option<P> {
    let a = v?.as_array()?;
    Some([a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32])
}
