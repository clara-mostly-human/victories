//! The events a tick produces, and which of them each fighter is told about. Ported from
//! the event pushes in `step.ts` and from `visibleEvents` in `observe.ts`.
//!
//! Events do not affect the match state. A search never asks for them (`Engine::step`);
//! an arena does (`Engine::step_events`), so its bots see exactly what the ladder would send.

use crate::geom::P;
use crate::map::PadKind;
use crate::observe::can_see;
use crate::state::State;
use crate::Engine;
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    InvalidState,
    ConflictingMoves,
    BadDir,
    MissingTarget,
    BadTarget,
    UnknownAttack,
    NotReady,
    Blocked,
    OutOfRange,
    NotVisible,
    BadText,
}

impl Reason {
    pub fn name(self) -> &'static str {
        match self {
            Reason::InvalidState => "invalid_state",
            Reason::ConflictingMoves => "conflicting_moves",
            Reason::BadDir => "bad_dir",
            Reason::MissingTarget => "missing_target",
            Reason::BadTarget => "bad_target",
            Reason::UnknownAttack => "unknown_attack",
            Reason::NotReady => "not_ready",
            Reason::Blocked => "blocked",
            Reason::OutOfRange => "out_of_range",
            Reason::NotVisible => "not_visible",
            Reason::BadText => "bad_text",
        }
    }
}

/// Fighters are named by id (1 or 2), as on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Rejected { fighter: u8, part: &'static str, reason: Reason },
    Shout { fighter: u8 },
    Dash { fighter: u8, from: P, to: P },
    Pickup { fighter: u8, pad: u8, kind: PadKind, pos: P },
    Attack { fighter: u8, attack: u8, target: P },
    Repulse { owner: u8, attack: u8, pos: P, tiles: Vec<P> },
    Hit { attacker: u8, target: u8, attack: u8, damage: i32, ricochet: bool },
    WallSlam { attacker: u8, target: u8, damage: i32 },
    Stunned { fighter: u8, ticks: i32 },
    Collision { attacker: u8, fighters: [u8; 2], damage: i32 },
    Knockback { attacker: u8, target: u8, from: P, to: P },
    AreaSpawn { area: u32, owner: u8, attack: u8, pos: P, explodes_tick: i32, radius: i32 },
    ProjectileSpawn { attack: u8, projectile: u32, owner: u8, pos: P, target: P },
    ProjectileBounce { projectile: u32, owner: u8, pos: P, wall: P, target: P },
    ProjectileEnd { projectile: u32, pos: P, reason: &'static str },
    AreaExplode { area: u32, owner: u8, attack: u8, pos: P, tiles: Vec<P> },
    ZoneDamage { fighter: u8, damage: i32 },
    ZoneShrink { radius: i32 },
    Death { fighter: u8 },
    BuffEnd { fighter: u8 },
    PickupSpawn { pad: u8, kind: PadKind, pos: P },
}

impl Event {
    /// The event as the server writes it. A shout's text is not kept by this port, so it
    /// is written as null.
    pub fn to_json(&self, e: &Engine) -> Value {
        let atk = |a: &u8| e.rules.attacks[*a as usize].name.clone();
        match self {
            Event::Rejected { fighter, part, reason } => json!({ "kind": "rejected", "fighter": fighter, "part": part, "reason": reason.name() }),
            Event::Shout { fighter } => json!({ "kind": "shout", "fighter": fighter, "text": null }),
            Event::Dash { fighter, from, to } => json!({ "kind": "dash", "fighter": fighter, "from": from, "to": to }),
            Event::Pickup { fighter, pad, kind, pos } => json!({ "kind": "pickup", "fighter": fighter, "pad": pad, "pickup": kind.name(), "pos": pos }),
            Event::Attack { fighter, attack, target } => json!({ "kind": "attack", "fighter": fighter, "attack": atk(attack), "target": target }),
            Event::Repulse { owner, attack, pos, tiles } => json!({ "kind": "repulse", "owner": owner, "attack": atk(attack), "pos": pos, "tiles": tiles }),
            Event::Hit { attacker, target, attack, damage, ricochet } => {
                let mut v = json!({ "kind": "hit", "attacker": attacker, "target": target, "attack": atk(attack), "damage": damage });
                if *ricochet {
                    v["ricochet"] = json!(true);
                }
                v
            }
            Event::WallSlam { attacker, target, damage } => json!({ "kind": "wall_slam", "attacker": attacker, "target": target, "damage": damage }),
            Event::Stunned { fighter, ticks } => json!({ "kind": "stunned", "fighter": fighter, "ticks": ticks }),
            Event::Collision { attacker, fighters, damage } => json!({ "kind": "collision", "attacker": attacker, "fighters": fighters, "damage": damage }),
            Event::Knockback { attacker, target, from, to } => json!({ "kind": "knockback", "attacker": attacker, "target": target, "from": from, "to": to }),
            Event::AreaSpawn { area, owner, attack, pos, explodes_tick, radius } => {
                json!({ "kind": "area_spawn", "area": area, "owner": owner, "attack": atk(attack), "pos": pos, "explodes_tick": explodes_tick, "radius": radius })
            }
            Event::ProjectileSpawn { attack, projectile, owner, pos, target } => {
                json!({ "kind": "projectile_spawn", "attack": atk(attack), "projectile": projectile, "owner": owner, "pos": pos, "target": target })
            }
            Event::ProjectileBounce { projectile, owner, pos, wall, target } => {
                json!({ "kind": "projectile_bounce", "projectile": projectile, "owner": owner, "pos": pos, "wall": wall, "target": target })
            }
            Event::ProjectileEnd { projectile, pos, reason } => json!({ "kind": "projectile_end", "projectile": projectile, "pos": pos, "reason": reason }),
            Event::AreaExplode { area, owner, attack, pos, tiles } => {
                json!({ "kind": "area_explode", "area": area, "owner": owner, "attack": atk(attack), "pos": pos, "tiles": tiles })
            }
            Event::ZoneDamage { fighter, damage } => json!({ "kind": "zone_damage", "fighter": fighter, "damage": damage }),
            Event::ZoneShrink { radius } => json!({ "kind": "zone_shrink", "radius": radius }),
            Event::Death { fighter } => json!({ "kind": "death", "fighter": fighter }),
            Event::BuffEnd { fighter } => json!({ "kind": "buff_end", "fighter": fighter, "buff": "power" }),
            Event::PickupSpawn { pad, kind, pos } => json!({ "kind": "pickup_spawn", "pad": pad, "pickup": kind.name(), "pos": pos }),
        }
    }
}

/// The events fighter index `viewer` is told about: its own, those involving a fighter it
/// can see, and global ones. `s` is the state after the tick the events came from.
pub fn visible_events<'a>(e: &Engine, s: &State, viewer: usize, events: &'a [Event]) -> Vec<&'a Event> {
    let my_id = (viewer + 1) as u8;
    let from = s.fighters[viewer].pos;
    let see_tile = |p: P| can_see(&e.rules, &e.map, from, p);
    let sees = |id: u8| id == my_id || ((1..=2).contains(&id) && see_tile(s.fighters[(id - 1) as usize].pos));
    events
        .iter()
        .filter(|ev| match ev {
            Event::Rejected { fighter, .. } => *fighter == my_id,
            Event::ZoneShrink { .. } | Event::Shout { .. } => true,
            Event::PickupSpawn { pos, .. } => see_tile(*pos),
            Event::Hit { attacker, target, .. } | Event::Knockback { attacker, target, .. } | Event::WallSlam { attacker, target, .. } => {
                sees(*attacker) || sees(*target)
            }
            Event::Collision { fighters, .. } => fighters.iter().any(|f| sees(*f)),
            Event::AreaSpawn { owner, pos, .. }
            | Event::AreaExplode { owner, pos, .. }
            | Event::ProjectileSpawn { owner, pos, .. }
            | Event::ProjectileBounce { owner, pos, .. }
            | Event::Repulse { owner, pos, .. } => *owner == my_id || see_tile(*pos),
            Event::ProjectileEnd { projectile, pos, .. } => {
                see_tile(*pos)
                    || events.iter().any(|x| matches!(x, Event::ProjectileSpawn { projectile: p, owner, .. } if p == projectile && *owner == my_id))
            }
            Event::Dash { fighter, .. }
            | Event::Pickup { fighter, .. }
            | Event::Attack { fighter, .. }
            | Event::Stunned { fighter, .. }
            | Event::ZoneDamage { fighter, .. }
            | Event::Death { fighter }
            | Event::BuffEnd { fighter } => sees(*fighter),
        })
        .collect()
}
