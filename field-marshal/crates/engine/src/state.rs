//! Match state. Flat and `Copy`, so a search can clone it with a memcpy.
//!
//! Fighters are stored by index: fighter id 1 is index 0, id 2 is index 1. Each fighter
//! is its own team, as in every ladder match.

use crate::geom::P;
use crate::map::MAX_PADS;
use crate::rules::MAX_ATTACKS;

pub const MAX_PROJECTILES: usize = 16;
pub const MAX_AREAS: usize = 6;

/// "No power buff" marker for `Fighter::power_until`.
pub const NO_BUFF: i32 = i32::MIN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FState {
    Idle,
    Dashing,
    Recovering,
    Stunned,
    Dead,
}

impl FState {
    pub fn name(self) -> &'static str {
        match self {
            FState::Idle => "idle",
            FState::Dashing => "dashing",
            FState::Recovering => "recovering",
            FState::Stunned => "stunned",
            FState::Dead => "dead",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fighter {
    pub pos: P,
    pub hp: i32,
    pub state: FState,
    pub state_end: i32,
    pub cd_move: i32,
    pub cd_dash: i32,
    pub cd_shout: i32,
    /// indexed like `Rules::attacks`
    pub cd_attack: [i32; MAX_ATTACKS],
    pub ticks_outside: i32,
    pub invulnerable: bool,
    pub alive: bool,
    pub damage_dealt: i32,
    /// last tick the power buff is active, or `NO_BUFF`
    pub power_until: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Projectile {
    pub id: u32,
    /// fighter index (0 or 1)
    pub owner: u8,
    /// index into `Rules::attacks`
    pub attack: u8,
    pub origin: P,
    pub target: P,
    pub traveled: i32,
    pub max_travel: i32,
    pub pos: P,
    pub exact_pos: P,
    pub speed: i32,
    pub damage: i32,
    pub bounces_left: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub id: u32,
    pub owner: u8,
    pub attack: u8,
    pub pos: P,
    pub origin: P,
    pub launched_tick: i32,
    pub explodes_tick: i32,
    pub radius: i32,
    pub damage: i32,
    pub slow: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pad {
    pub available: bool,
    pub respawn_tick: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    LastStanding,
    AllDead,
    TimeoutHp,
    TimeoutDamage,
    TimeoutDraw,
}

impl EndReason {
    pub fn name(self) -> &'static str {
        match self {
            EndReason::LastStanding => "last_standing",
            EndReason::AllDead => "all_dead",
            EndReason::TimeoutHp => "timeout_hp",
            EndReason::TimeoutDamage => "timeout_damage",
            EndReason::TimeoutDraw => "timeout_draw",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchResult {
    /// winning fighter id (1 or 2), or `None` for a draw
    pub winner: Option<u8>,
    pub reason: EndReason,
    pub final_tick: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub tick: i32,
    pub fighters: [Fighter; 2],
    pub projectiles: [Projectile; MAX_PROJECTILES],
    pub n_projectiles: u8,
    pub areas: [Area; MAX_AREAS],
    pub n_areas: u8,
    pub pads: [Pad; MAX_PADS],
    pub next_projectile_id: u32,
    pub zone_radius: i32,
    pub result: Option<MatchResult>,
    /// kept only so the state hash matches the official one; the engine never draws again
    pub seed: i64,
    pub rng: u64,
}

impl State {
    #[inline]
    pub fn projectiles(&self) -> &[Projectile] {
        &self.projectiles[..self.n_projectiles as usize]
    }
    #[inline]
    pub fn areas(&self) -> &[Area] {
        &self.areas[..self.n_areas as usize]
    }
}

pub const EMPTY_PROJECTILE: Projectile = Projectile {
    id: 0,
    owner: 0,
    attack: 0,
    origin: [0, 0],
    target: [0, 0],
    traveled: 0,
    max_travel: 0,
    pos: [0, 0],
    exact_pos: [0, 0],
    speed: 0,
    damage: 0,
    bounces_left: 0,
};

pub const EMPTY_AREA: Area = Area {
    id: 0,
    owner: 0,
    attack: 0,
    pos: [0, 0],
    origin: [0, 0],
    launched_tick: 0,
    explodes_tick: 0,
    radius: 0,
    damage: 0,
    slow: 0,
};
