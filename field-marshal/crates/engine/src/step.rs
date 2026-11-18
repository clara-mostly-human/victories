//! Tick resolution, ported from `packages/engine/src/step.ts`, `validate.ts`, `flight.ts`
//! and `match.ts`. The eight phases run in the same order and loop over fighters in the
//! same order (id 1, then id 2) as the official engine.
//!
//! One simplification is safe and used throughout: a match ends the tick a fighter dies,
//! and `step` refuses a finished match, so both fighters are alive in phases 1 to 6.

use crate::events::{Event, Reason};
use crate::geom::{add, chebyshev, octant, ray_point, ray_tiles, sub_to_tile, Ray, ShotBuf, DIR_VEC, P, SUB};
use crate::map::{MapData, PadKind};
use crate::rules::{Attack, AttackKind, Rules};
use crate::state::*;
use crate::turn::{AttackIn, DirIn, RawTurn, ShoutIn, TargetIn};

/// `max_travel` of a shot with no range limit: far longer than any map.
pub const UNLIMITED_TRAVEL: i64 = 1_000_000 * SUB;

/// What happened to each fighter during one tick, for a search to score without
/// diffing states. Index by fighter index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TickSummary {
    /// hp lost this tick, from any source
    pub hp_lost: [i32; 2],
    /// slammed into a wall this tick
    pub slammed: [bool; 2],
}

/// A validated turn: what the engine will actually play.
#[derive(Clone, Copy, Debug, Default)]
struct Valid {
    mv: Option<u8>,
    dash: Option<u8>,
    /// (attack index, target); the target is unused for a burst
    attack: Option<(u8, P)>,
    shout: bool,
}

#[derive(Clone, Copy)]
struct Fired {
    f: usize,
    attack: u8,
    target: P,
    direction: P,
}

enum Flight {
    /// ended on a fighter standing on this tile
    Hit(P),
    /// ended on this wall tile
    Wall(P),
    /// reflected off this wall tile
    Bounce(P),
    Range,
    Flying,
}

/// What a tick records as it resolves: always the summary, and the event list when asked.
struct Rec<'a> {
    sum: TickSummary,
    events: Option<&'a mut Vec<Event>>,
}

impl Rec<'_> {
    #[inline]
    fn on(&self) -> bool {
        self.events.is_some()
    }
    #[inline]
    fn emit(&mut self, make: impl FnOnce() -> Event) {
        if let Some(ev) = self.events.as_mut() {
            ev.push(make());
        }
    }
}

#[inline]
fn hit_damage(base: i32, armor: i32) -> i32 {
    (base - armor).max(1)
}

fn validate(rules: &Rules, map: &MapData, s: &State, i: usize, t: &RawTurn, rec: &mut Rec) -> Valid {
    let f = &s.fighters[i];
    let id = (i + 1) as u8;
    let mut v = Valid::default();
    let reject = |rec: &mut Rec, part: &'static str, reason: Reason| rec.emit(|| Event::Rejected { fighter: id, part, reason });
    // Talking takes no action, so a fighter can shout while it recovers or is stunned.
    match t.shout {
        ShoutIn::None => {}
        ShoutIn::Bad => reject(rec, "shout", Reason::BadText),
        ShoutIn::Text { chars } => {
            let max = rules.shout.map(|x| x.max_length).unwrap_or(0);
            if chars as i64 > max as i64 {
                reject(rec, "shout", Reason::BadText);
            } else if f.cd_shout > 0 {
                reject(rec, "shout", Reason::NotReady);
            } else {
                v.shout = true;
            }
        }
    }
    let has_move = t.mv != DirIn::None;
    let has_dash = t.dash != DirIn::None;
    let has_attack = t.attack != AttackIn::None;
    if !has_move && !has_dash && !has_attack {
        return v;
    }
    if f.state != FState::Idle {
        reject(rec, "turn", Reason::InvalidState);
        return v;
    }
    if has_move && has_dash {
        reject(rec, "turn", Reason::ConflictingMoves);
    } else {
        match t.mv {
            DirIn::None => {}
            DirIn::Bad => reject(rec, "move", Reason::BadDir),
            DirIn::Dir(d) => {
                if f.cd_move > 0 {
                    reject(rec, "move", Reason::NotReady);
                } else if map.step_blocked(f.pos, d) {
                    reject(rec, "move", Reason::Blocked);
                } else {
                    v.mv = Some(d);
                }
            }
        }
        match t.dash {
            DirIn::None => {}
            DirIn::Bad => reject(rec, "dash", Reason::BadDir),
            DirIn::Dir(d) => {
                if f.cd_dash > 0 {
                    reject(rec, "dash", Reason::NotReady);
                } else {
                    v.dash = Some(d);
                }
            }
        }
    }
    match t.attack {
        AttackIn::None => {}
        AttackIn::Unknown => reject(rec, "attack", Reason::UnknownAttack),
        AttackIn::Named { attack, target } => {
            if f.cd_attack[attack as usize] > 0 {
                reject(rec, "attack", Reason::NotReady);
            } else if rules.attacks[attack as usize].is_burst() {
                v.attack = Some((attack, [0, 0]));
            } else {
                match target {
                    TargetIn::Missing => reject(rec, "attack", Reason::MissingTarget),
                    TargetIn::Bad => reject(rec, "attack", Reason::BadTarget),
                    TargetIn::Tile(p) => v.attack = Some((attack, p)),
                }
            }
        }
    }
    v
}

/// Resolves simultaneous one-tile moves. `to[i]` is where fighter `i` wants to go (None
/// if it is not moving); `pos` is where everyone stands. Returns who actually moves.
/// Two movers into one tile: neither. Swaps: neither. Moving into a tile whose occupant
/// stays (including a mover that was itself stopped): stopped.
fn resolve_simultaneous(to: &[Option<P>; 2], pos: &[P; 2]) -> [bool; 2] {
    let mut ok = [to[0].is_some(), to[1].is_some()];
    let occupant = |p: P| -> Option<usize> {
        // Mirrors the engine's Map: a later fighter on the same tile would replace an earlier one.
        if pos[1] == p {
            Some(1)
        } else if pos[0] == p {
            Some(0)
        } else {
            None
        }
    };
    let mut contested = [false; 2];
    if let (Some(a), Some(b)) = (to[0], to[1]) {
        if a == b {
            contested = [true, true];
        }
    }
    for i in 0..2 {
        if let Some(dest) = to[i] {
            if let Some(o) = occupant(dest) {
                if let Some(o_dest) = to[o] {
                    if o_dest == pos[i] {
                        contested[i] = true;
                    }
                }
            }
        }
    }
    for i in 0..2 {
        if contested[i] {
            ok[i] = false;
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..2 {
            if !ok[i] {
                continue;
            }
            if let Some(o) = occupant(to[i].unwrap()) {
                if o != i && !ok[o] {
                    ok[i] = false;
                    changed = true;
                }
            }
        }
    }
    ok
}

/// Applies damage and credits the attacker with the hp actually removed. Weapon damage
/// (attacker set) is scaled by the attacker's power buff.
fn damage(rules: &Rules, s: &mut State, rec: &mut Rec, target: usize, mut amount: i32, attacker: Option<usize>) -> i32 {
    if let Some(a) = attacker {
        // A weapon cannot damage its owner (or a teammate; each fighter is its own team).
        if a == target {
            return 0;
        }
        if let Some(power) = rules.pickups.as_ref().and_then(|p| p.power) {
            if s.fighters[a].power_until != NO_BUFF {
                amount = (amount as i64 * power.mul_num as i64).div_euclid(power.mul_den as i64) as i32;
            }
        }
    }
    let dealt = s.fighters[target].hp.min(amount);
    s.fighters[target].hp -= dealt;
    rec.sum.hp_lost[target] += dealt;
    if let Some(a) = attacker {
        s.fighters[a].damage_dealt += dealt;
    }
    dealt
}

/// Extra move cooldown from slow areas: an opponent's area slows a fighter standing in
/// it from the tick after it is cast until the tick it explodes; the worst one counts.
pub fn slow_penalty(map: &MapData, s: &State, i: usize, tick: i32) -> i32 {
    let mut penalty = 0;
    let pos = s.fighters[i].pos;
    for b in s.areas() {
        if b.slow <= penalty || tick <= b.launched_tick || tick > b.explodes_tick {
            continue;
        }
        if b.owner as usize == i || !map.in_blast(b.pos, b.radius, pos) {
            continue;
        }
        penalty = b.slow;
    }
    penalty
}

/// Why an aimed attack can't be fired at `target` from where the fighter stands, or `None`.
fn target_problem(rules: &Rules, map: &MapData, pos: P, atk: &Attack, target: P) -> Option<Reason> {
    if target == pos {
        return Some(Reason::BadTarget);
    }
    if let AttackKind::Area { range, .. } = atk.kind {
        if chebyshev(pos, target) > range {
            return Some(Reason::OutOfRange);
        }
        if map.is_wall(target) {
            return Some(Reason::Blocked);
        }
        if !(chebyshev(pos, target) <= rules.sight_radius && map.has_line_of_sight(pos, target)) {
            return Some(Reason::NotVisible);
        }
    }
    None
}

/// Shoves fighter `t` up to `tiles` steps in `dir`, one at a time. A wall (or a corner it
/// can't cut past) stops it with a slam: damage and a stun. Another fighter stops it
/// with a collision that hurts both.
fn push(rules: &Rules, map: &MapData, s: &mut State, rec: &mut Rec, attacker: usize, t: usize, dir: u8, tiles: i32, tick: i32) {
    let from = s.fighters[t].pos;
    let (a_id, t_id) = ((attacker + 1) as u8, (t + 1) as u8);
    for _ in 0..tiles {
        if map.step_blocked(s.fighters[t].pos, dir) {
            let dealt = damage(rules, s, rec, t, rules.wall_slam_damage, Some(attacker));
            rec.emit(|| Event::WallSlam { attacker: a_id, target: t_id, damage: dealt });
            rec.sum.slammed[t] = true;
            let until = tick + rules.wall_slam_stun;
            let f = &mut s.fighters[t];
            f.state_end = if f.state == FState::Idle { until } else { f.state_end.max(until) };
            f.state = FState::Stunned;
            rec.emit(|| Event::Stunned { fighter: t_id, ticks: rules.wall_slam_stun });
            break;
        }
        let next = add(s.fighters[t].pos, DIR_VEC[dir as usize]);
        let o = 1 - t;
        if s.fighters[o].alive && s.fighters[o].pos == next {
            let dmg = rules.fighter_collision_damage;
            damage(rules, s, rec, t, dmg, Some(attacker));
            damage(rules, s, rec, o, dmg, Some(attacker));
            rec.emit(|| Event::Collision { attacker: a_id, fighters: [t_id, (o + 1) as u8], damage: dmg });
            break;
        }
        s.fighters[t].pos = next;
    }
    let to = s.fighters[t].pos;
    if from != to {
        rec.emit(|| Event::Knockback { attacker: a_id, target: t_id, from, to });
    }
}

/// Advances `p` one tick along its line, mutating it. The segment swept includes the tile
/// the projectile starts the tick in. `stop_at` is asked about each open tile in order;
/// returning true ends the flight there.
fn advance_projectile(map: &MapData, p: &mut Projectile, buf: &mut ShotBuf, mut stop_at: impl FnMut(P) -> bool) -> Flight {
    let ray = Ray::new(p.origin, p.target);
    let to = (p.traveled as i64 + p.speed as i64 * SUB).min(p.max_travel as i64);
    let mut last_open = p.pos;
    ray_tiles(&ray, p.traveled as i64, to, buf);
    for rt in buf.as_slice() {
        let tile = rt.tile;
        if map.is_wall(tile) {
            let wx = tile[0] - last_open[0];
            let wy = tile[1] - last_open[1];
            if p.bounces_left > 0 && wx.abs() + wy.abs() == 1 {
                let dx = p.target[0] - p.origin[0];
                let dy = p.target[1] - p.origin[1];
                let remaining = p.max_travel as i64 - rt.entry.ceil();
                if remaining > 0 {
                    p.origin = last_open;
                    p.target = [
                        last_open[0] + if wx != 0 { -dx } else { dx },
                        last_open[1] + if wy != 0 { -dy } else { dy },
                    ];
                    p.pos = last_open;
                    p.exact_pos = [last_open[0] * SUB as i32, last_open[1] * SUB as i32];
                    p.max_travel = remaining as i32;
                    p.traveled = 0;
                    p.bounces_left -= 1;
                    return Flight::Bounce(tile);
                }
            }
            return Flight::Wall(tile);
        }
        if stop_at(tile) {
            p.pos = tile;
            return Flight::Hit(tile);
        }
        last_open = tile;
    }
    p.traveled = to as i32;
    let e = ray_point(&ray, to);
    p.exact_pos = [e[0] as i32, e[1] as i32];
    let t = sub_to_tile(e);
    p.pos = [t[0] as i32, t[1] as i32];
    if to >= p.max_travel as i64 {
        Flight::Range
    } else {
        Flight::Flying
    }
}

/// One tick of flight for a danger prediction: `record` is told each open tile the shot
/// sweeps, fighters are ignored. Returns whether the shot is still flying afterwards.
pub(crate) fn advance_for_danger(map: &MapData, p: &mut Projectile, buf: &mut ShotBuf, mut record: impl FnMut(P)) -> bool {
    let flight = advance_projectile(map, p, buf, |tile| {
        record(tile);
        false
    });
    matches!(flight, Flight::Flying | Flight::Bounce(_))
}

/// Safe-zone Chebyshev radius in effect after phase 6 of `tick`.
pub fn zone_radius_at(rules: &Rules, map: &MapData, tick: i32) -> i32 {
    let z = &rules.zone;
    let [cx, cy] = map.center;
    let full = cx.max(map.w - 1 - cx).max(cy).max(map.h - 1 - cy);
    if tick < z.start_tick {
        return full;
    }
    let initial = (map.w.max(map.h) + 1).div_euclid(2);
    let shrinks = (tick - z.start_tick).div_euclid(z.interval);
    full.min(z.min_radius.max(initial - z.step * shrinks))
}

fn check_end(rules: &Rules, s: &State, resolved_tick: i32) -> Option<MatchResult> {
    let f = &s.fighters;
    let alive = [f[0].alive, f[1].alive];
    match alive {
        [true, false] => return Some(MatchResult { winner: Some(1), reason: EndReason::LastStanding, final_tick: resolved_tick }),
        [false, true] => return Some(MatchResult { winner: Some(2), reason: EndReason::LastStanding, final_tick: resolved_tick }),
        [false, false] => return Some(MatchResult { winner: None, reason: EndReason::AllDead, final_tick: resolved_tick }),
        [true, true] => {}
    }
    if resolved_tick + 1 < rules.max_ticks {
        return None;
    }
    // Timeout: higher hp wins, then more damage dealt, else a draw.
    Some(if f[0].hp != f[1].hp {
        MatchResult { winner: Some(if f[0].hp > f[1].hp { 1 } else { 2 }), reason: EndReason::TimeoutHp, final_tick: resolved_tick }
    } else if f[0].damage_dealt != f[1].damage_dealt {
        MatchResult {
            winner: Some(if f[0].damage_dealt > f[1].damage_dealt { 1 } else { 2 }),
            reason: EndReason::TimeoutDamage,
            final_tick: resolved_tick,
        }
    } else {
        MatchResult { winner: None, reason: EndReason::TimeoutDraw, final_tick: resolved_tick }
    })
}

/// Resolves one tick in place. `turns[i]` is what fighter index `i` asked for.
pub fn step(rules: &Rules, map: &MapData, s: &mut State, turns: &[RawTurn; 2]) -> TickSummary {
    step_impl(rules, map, s, turns, None)
}

/// As `step`, and also writes the tick's events, in the official order, into `events`
/// (which is cleared first).
pub fn step_events(rules: &Rules, map: &MapData, s: &mut State, turns: &[RawTurn; 2], events: &mut Vec<Event>) -> TickSummary {
    events.clear();
    step_impl(rules, map, s, turns, Some(events))
}

fn step_impl(rules: &Rules, map: &MapData, s: &mut State, turns: &[RawTurn; 2], events: Option<&mut Vec<Event>>) -> TickSummary {
    assert!(s.result.is_none(), "match is over");
    let tick = s.tick;
    let mut rec = Rec { sum: TickSummary::default(), events };
    let rec = &mut rec;

    // ---------------------------------------------------------------- phase 1: validate
    let mut v = [Valid::default(); 2];
    for i in 0..2 {
        v[i] = validate(rules, map, s, i, &turns[i], rec);
        if v[i].shout {
            s.fighters[i].cd_shout = rules.shout.map(|x| x.cooldown).unwrap_or(0);
            rec.emit(|| Event::Shout { fighter: (i + 1) as u8 });
        }
    }

    // ---------------------------------------------------------------- phase 2: dashes
    if v[0].dash.is_some() || v[1].dash.is_some() {
        let dashers = [v[0].dash.is_some(), v[1].dash.is_some()];
        let start = [s.fighters[0].pos, s.fighters[1].pos];
        let mut active = dashers;
        for i in 0..2 {
            if active[i] {
                let f = &mut s.fighters[i];
                f.state = FState::Dashing;
                f.invulnerable = true;
                f.cd_dash = rules.dash_cooldown;
            }
        }
        // Step every dasher one tile at a time, all at once. A dash stops at the first
        // wall, map edge or fighter; two dashes meeting stop one tile short of each other.
        let mut k = 0;
        while k < rules.dash_distance && (active[0] || active[1]) {
            let mut to: [Option<P>; 2] = [None, None];
            for i in 0..2 {
                if !active[i] {
                    continue;
                }
                let dir = v[i].dash.unwrap();
                if map.step_blocked(s.fighters[i].pos, dir) {
                    active[i] = false;
                    continue;
                }
                to[i] = Some(add(s.fighters[i].pos, DIR_VEC[dir as usize]));
            }
            let ok = resolve_simultaneous(&to, &[s.fighters[0].pos, s.fighters[1].pos]);
            for i in 0..2 {
                if let Some(dest) = to[i] {
                    if ok[i] {
                        s.fighters[i].pos = dest;
                    } else {
                        active[i] = false;
                    }
                }
            }
            k += 1;
        }
        for i in 0..2 {
            if dashers[i] {
                rec.emit(|| Event::Dash { fighter: (i + 1) as u8, from: start[i], to: s.fighters[i].pos });
            }
        }
    }

    // ---------------------------------------------------------------- phase 3: movement
    {
        let mut to: [Option<P>; 2] = [None, None];
        for i in 0..2 {
            if let Some(dir) = v[i].mv {
                s.fighters[i].cd_move = rules.move_cooldown + slow_penalty(map, s, i, tick);
                to[i] = Some(add(s.fighters[i].pos, DIR_VEC[dir as usize]));
            }
        }
        if to[0].is_some() || to[1].is_some() {
            let ok = resolve_simultaneous(&to, &[s.fighters[0].pos, s.fighters[1].pos]);
            for i in 0..2 {
                if ok[i] {
                    s.fighters[i].pos = to[i].unwrap();
                }
            }
        }
    }

    // ---------------------------------------------------------------- phase 4: pickups
    if let Some(pr) = rules.pickups.as_ref() {
        for (pi, def) in map.pads.iter().enumerate() {
            if !s.pads[pi].available {
                continue;
            }
            let i = if s.fighters[0].pos == def.pos {
                0
            } else if s.fighters[1].pos == def.pos {
                1
            } else {
                continue;
            };
            s.pads[pi].available = false;
            rec.emit(|| Event::Pickup { fighter: (i + 1) as u8, pad: (pi + 1) as u8, kind: def.kind, pos: def.pos });
            match def.kind {
                PadKind::Health => {
                    let h = pr.health.expect("map places a health pickup the rules don't define");
                    s.pads[pi].respawn_tick = tick + h.respawn;
                    let f = &mut s.fighters[i];
                    f.hp = rules.max_hp.min(f.hp + h.amount);
                }
                PadKind::Power => {
                    let p = pr.power.expect("map places a power pickup the rules don't define");
                    s.pads[pi].respawn_tick = tick + p.respawn;
                    // Taking a buff you already have refreshes it; it never shortens it.
                    let f = &mut s.fighters[i];
                    let cur = if f.power_until == NO_BUFF { -1 } else { f.power_until };
                    f.power_until = cur.max(tick + p.duration - 1);
                }
            }
        }
    }

    // ---------------------------------------------------------------- phase 5: attacks
    let mut fired: [Option<Fired>; 2] = [None, None];
    let mut n_fired = 0;
    for i in 0..2 {
        if let Some((ai, tgt)) = v[i].attack {
            let atk = &rules.attacks[ai as usize];
            let pos = s.fighters[i].pos;
            // The repulsor has no aim. Aimed attacks are judged from where the fighter
            // ended its movement.
            let target = if atk.is_burst() { pos } else { tgt };
            if !atk.is_burst() {
                if let Some(reason) = target_problem(rules, map, pos, atk, target) {
                    rec.emit(|| Event::Rejected { fighter: (i + 1) as u8, part: "attack", reason });
                    continue;
                }
            }
            // Each weapon has its own cooldown: firing one leaves the others ready.
            s.fighters[i].cd_attack[ai as usize] = atk.cooldown;
            rec.emit(|| Event::Attack { fighter: (i + 1) as u8, attack: ai, target });
            fired[n_fired] = Some(Fired { f: i, attack: ai, target, direction: [target[0] - pos[0], target[1] - pos[1]] });
            n_fired += 1;
        }
    }
    if n_fired > 0 {
        // Every repulsor's ring is fixed before any push moves anyone.
        let mut hits = [false; 2];
        for (k, fr) in fired.iter().enumerate() {
            if let Some(fr) = fr {
                if rules.attacks[fr.attack as usize].is_burst() {
                    let t = &s.fighters[1 - fr.f];
                    hits[k] = t.alive && !t.invulnerable && chebyshev(s.fighters[fr.f].pos, t.pos) == 1;
                    if rec.on() {
                        let pos = s.fighters[fr.f].pos;
                        let tiles: Vec<P> = DIR_VEC.iter().map(|d| add(pos, *d)).filter(|p| !map.is_wall(*p)).collect();
                        rec.emit(|| Event::Repulse { owner: (fr.f + 1) as u8, attack: fr.attack, pos, tiles });
                    }
                }
            }
        }
        for (k, fr) in fired.iter().enumerate() {
            if let Some(fr) = fr {
                let atk = &rules.attacks[fr.attack as usize];
                if let AttackKind::Burst { push: push_tiles } = atk.kind {
                    if hits[k] {
                        let t = 1 - fr.f;
                        let dealt = damage(rules, s, rec, t, hit_damage(atk.damage, rules.armor), Some(fr.f));
                        rec.emit(|| Event::Hit { attacker: (fr.f + 1) as u8, target: (t + 1) as u8, attack: fr.attack, damage: dealt, ricochet: false });
                        if let Some(dir) = octant(s.fighters[fr.f].pos, s.fighters[t].pos) {
                            push(rules, map, s, rec, fr.f, t, dir, push_tiles, tick);
                        }
                    }
                }
            }
        }
        for fr in fired.iter().flatten() {
            let atk = &rules.attacks[fr.attack as usize];
            let pos = s.fighters[fr.f].pos;
            match atk.kind {
                AttackKind::Area { fuse, radius, slow, .. } => {
                    let n = s.n_areas as usize;
                    assert!(n < MAX_AREAS, "too many areas for this port");
                    s.areas[n] = Area {
                        id: s.next_projectile_id,
                        owner: fr.f as u8,
                        attack: fr.attack,
                        pos: fr.target,
                        origin: pos,
                        launched_tick: tick,
                        explodes_tick: tick + fuse,
                        radius,
                        damage: atk.damage,
                        slow,
                    };
                    s.n_areas += 1;
                    s.next_projectile_id += 1;
                    let b = s.areas[n];
                    rec.emit(|| Event::AreaSpawn { area: b.id, owner: b.owner + 1, attack: b.attack, pos: b.pos, explodes_tick: b.explodes_tick, radius: b.radius });
                }
                AttackKind::Burst { .. } => {}
                AttackKind::Projectile { range, speed, bounces } => {
                    // The target sets the aim after movement; knockback shifts the launch
                    // point, not the direction.
                    let n = s.n_projectiles as usize;
                    assert!(n < MAX_PROJECTILES, "too many projectiles for this port");
                    s.projectiles[n] = Projectile {
                        id: s.next_projectile_id,
                        owner: fr.f as u8,
                        attack: fr.attack,
                        origin: pos,
                        target: add(pos, fr.direction),
                        traveled: 0,
                        max_travel: match range {
                            None => UNLIMITED_TRAVEL as i32,
                            Some(r) => r * SUB as i32,
                        },
                        pos,
                        exact_pos: [pos[0] * SUB as i32, pos[1] * SUB as i32],
                        speed,
                        damage: atk.damage,
                        bounces_left: bounces,
                    };
                    s.n_projectiles += 1;
                    s.next_projectile_id += 1;
                    let p = s.projectiles[n];
                    rec.emit(|| Event::ProjectileSpawn { attack: p.attack, projectile: p.id, owner: p.owner + 1, pos: p.origin, target: p.target });
                }
            }
        }
    }

    // Each projectile sweeps a straight segment this tick. It hits the first wall or
    // fighter whose tile interior the segment passes through, including the tile it
    // starts the tick in. Projectiles are stored in id order, the order they resolve in.
    if s.n_projectiles > 0 {
        let n = s.n_projectiles as usize;
        let mut kept = 0;
        let mut buf = ShotBuf::new();
        for j in 0..n {
            let mut p = s.projectiles[j];
            let owner = p.owner as usize;
            let mut hit: Option<usize> = None;
            let fighters = &s.fighters;
            let flight = advance_projectile(map, &mut p, &mut buf, |tile| {
                for (t, f) in fighters.iter().enumerate() {
                    if f.alive && t != owner && !f.invulnerable && f.pos == tile {
                        hit = Some(t);
                        return true;
                    }
                }
                false
            });
            match flight {
                Flight::Bounce(wall) => {
                    s.projectiles[kept] = p;
                    kept += 1;
                    rec.emit(|| Event::ProjectileBounce { projectile: p.id, owner: p.owner + 1, pos: p.pos, wall, target: p.target });
                }
                Flight::Flying => {
                    s.projectiles[kept] = p;
                    kept += 1;
                }
                Flight::Hit(tile) => {
                    let t = hit.unwrap();
                    let dealt = damage(rules, s, rec, t, hit_damage(p.damage, rules.armor), Some(owner));
                    // A hit after a wall bounce is a ricochet.
                    let ricochet = matches!(rules.attacks[p.attack as usize].kind, AttackKind::Projectile { bounces, .. } if p.bounces_left < bounces);
                    rec.emit(|| Event::Hit { attacker: p.owner + 1, target: (t + 1) as u8, attack: p.attack, damage: dealt, ricochet });
                    rec.emit(|| Event::ProjectileEnd { projectile: p.id, pos: tile, reason: "hit" });
                }
                Flight::Wall(wall) => rec.emit(|| Event::ProjectileEnd { projectile: p.id, pos: wall, reason: "wall" }),
                Flight::Range => rec.emit(|| Event::ProjectileEnd { projectile: p.id, pos: p.pos, reason: "range" }),
            }
        }
        for slot in s.projectiles[kept..n].iter_mut() {
            *slot = EMPTY_PROJECTILE;
        }
        s.n_projectiles = kept as u8;
    }

    if s.n_areas > 0 {
        let n = s.n_areas as usize;
        for j in 0..n {
            let b = s.areas[j];
            if b.explodes_tick != tick {
                continue;
            }
            if rec.on() {
                let tiles = crate::observe::blast_tiles(map, b.pos, b.radius);
                rec.emit(|| Event::AreaExplode { area: b.id, owner: b.owner + 1, attack: b.attack, pos: b.pos, tiles });
            }
            // Blasts affect opponents only, even if the shooter is inside the footprint.
            for i in 0..2 {
                let f = &s.fighters[i];
                if b.owner as usize == i || f.invulnerable || !map.in_blast(b.pos, b.radius, f.pos) {
                    continue;
                }
                let dealt = damage(rules, s, rec, i, hit_damage(b.damage, rules.armor), Some(b.owner as usize));
                rec.emit(|| Event::Hit { attacker: b.owner + 1, target: (i + 1) as u8, attack: b.attack, damage: dealt, ricochet: false });
            }
        }
        let mut kept = 0;
        for j in 0..n {
            if s.areas[j].explodes_tick > tick {
                s.areas[kept] = s.areas[j];
                kept += 1;
            }
        }
        for slot in s.areas[kept..n].iter_mut() {
            *slot = EMPTY_AREA;
        }
        s.n_areas = kept as u8;
    }

    // ---------------------------------------------------------------- phase 6: zone
    for i in 0..2 {
        if chebyshev(s.fighters[i].pos, map.center) > s.zone_radius {
            let z = &rules.zone;
            let amount = z.damage_base + z.damage_step * s.fighters[i].ticks_outside.div_euclid(z.damage_step_ticks);
            s.fighters[i].ticks_outside += 1;
            let dealt = damage(rules, s, rec, i, amount, None);
            rec.emit(|| Event::ZoneDamage { fighter: (i + 1) as u8, damage: dealt });
        } else {
            s.fighters[i].ticks_outside = 0;
        }
    }
    let radius = zone_radius_at(rules, map, tick);
    if radius != s.zone_radius {
        s.zone_radius = radius;
        rec.emit(|| Event::ZoneShrink { radius });
    }

    // ---------------------------------------------------------------- phase 7: deaths
    for (i, f) in s.fighters.iter_mut().enumerate() {
        if f.hp <= 0 {
            f.alive = false;
            f.state = FState::Dead;
            f.invulnerable = false;
            rec.emit(|| Event::Death { fighter: (i + 1) as u8 });
        }
    }

    // ---------------------------------------------------------------- phase 8: timers
    for (i, f) in s.fighters.iter_mut().enumerate() {
        if !f.alive {
            continue;
        }
        f.cd_move = (f.cd_move - 1).max(0);
        f.cd_dash = (f.cd_dash - 1).max(0);
        f.cd_shout = (f.cd_shout - 1).max(0);
        for c in f.cd_attack.iter_mut() {
            *c = (*c - 1).max(0);
        }
        if (f.state == FState::Recovering || f.state == FState::Stunned) && f.state_end <= tick {
            f.state = FState::Idle;
        }
        if f.state == FState::Dashing {
            if rules.dash_recovery > 0 {
                f.state = FState::Recovering;
                f.state_end = tick + rules.dash_recovery;
            } else {
                f.state = FState::Idle;
            }
        }
        f.invulnerable = false;
        if f.power_until != NO_BUFF && f.power_until <= tick {
            f.power_until = NO_BUFF;
            rec.emit(|| Event::BuffEnd { fighter: (i + 1) as u8 });
        }
    }
    // A pad restocks at the end of the tick before its respawn tick.
    for (pi, pad) in s.pads[..map.pads.len()].iter_mut().enumerate() {
        if !pad.available && pad.respawn_tick <= tick + 1 {
            pad.available = true;
            let def = map.pads[pi];
            rec.emit(|| Event::PickupSpawn { pad: (pi + 1) as u8, kind: def.kind, pos: def.pos });
        }
    }
    s.result = check_end(rules, s, tick);
    s.tick += 1;
    rec.sum
}
