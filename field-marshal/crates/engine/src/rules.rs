//! The rules table, ported from `packages/engine/src/rules.ts`.
//!
//! The ladder sends the rules in every `hello` and every replay header embeds them, so
//! nothing here is hard-coded to one rules version: the numbers are read from JSON.

use crate::hash::hash_value;
use serde_json::Value;

/// Most attacks a rules table may define (the shipped table has three).
pub const MAX_ATTACKS: usize = 4;

#[derive(Clone, Debug)]
pub enum AttackKind {
    Projectile { range: Option<i32>, speed: i32, bounces: i32 },
    Area { range: i32, fuse: i32, radius: i32, slow: i32 },
    Burst { push: i32 },
}

#[derive(Clone, Debug)]
pub struct Attack {
    pub name: String,
    pub damage: i32,
    pub cooldown: i32,
    pub kind: AttackKind,
}

impl Attack {
    #[inline]
    pub fn is_burst(&self) -> bool {
        matches!(self.kind, AttackKind::Burst { .. })
    }
}

#[derive(Clone, Debug)]
pub struct ZoneRules {
    pub start_tick: i32,
    pub interval: i32,
    pub step: i32,
    pub min_radius: i32,
    pub warning_ticks: i32,
    pub damage_base: i32,
    pub damage_step: i32,
    pub damage_step_ticks: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct HealthRules {
    pub amount: i32,
    pub respawn: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct PowerRules {
    pub duration: i32,
    pub mul_num: i32,
    pub mul_den: i32,
    pub respawn: i32,
}

#[derive(Clone, Debug)]
pub struct PickupRules {
    pub first_spawn: i32,
    pub health: Option<HealthRules>,
    pub power: Option<PowerRules>,
}

#[derive(Clone, Copy, Debug)]
pub struct ShoutRules {
    pub max_length: i32,
    pub cooldown: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub startup_ms: i32,
    pub tick_ms: i32,
    pub match_budget_ms: i32,
    pub max_consecutive_failures: i32,
}

/// Which cooldown a key of a fighter's `cooldowns` object refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CdSlot {
    Move,
    Dash,
    Shout,
    Attack(u8),
}

#[derive(Clone, Debug)]
pub struct Rules {
    pub version: String,
    pub max_ticks: i32,
    pub max_hp: i32,
    pub move_cooldown: i32,
    pub sight_radius: i32,
    pub armor: i32,
    /// in key order (alphabetical); a turn names an attack, the engine uses its index here
    pub attacks: Vec<Attack>,
    pub dash_distance: i32,
    pub dash_cooldown: i32,
    pub dash_recovery: i32,
    pub wall_slam_damage: i32,
    pub wall_slam_stun: i32,
    pub fighter_collision_damage: i32,
    pub zone: ZoneRules,
    pub pickups: Option<PickupRules>,
    pub shout: Option<ShoutRules>,
    pub limits: Limits,
    /// the keys of a fighter's `cooldowns` object, sorted as canonical JSON writes them
    pub cooldown_keys: Vec<(String, CdSlot)>,
    /// cyrb53 of the canonical JSON of the table, as `hello` and replay headers report it
    pub hash: String,
}

fn int(v: Option<&Value>, path: &str) -> Result<i32, String> {
    let v = v.ok_or_else(|| format!("rules: {path} missing"))?;
    let n = v
        .as_i64()
        .or_else(|| v.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
        .ok_or_else(|| format!("rules: {path} must be a non-negative integer, got {v}"))?;
    if !(0..=i32::MAX as i64).contains(&n) {
        return Err(format!("rules: {path} must be a non-negative integer, got {v}"));
    }
    Ok(n as i32)
}

fn opt_int(v: Option<&Value>, path: &str) -> Result<Option<i32>, String> {
    match v {
        None => Ok(None),
        Some(_) => int(v, path).map(Some),
    }
}

impl Rules {
    pub fn from_json(raw: &Value) -> Result<Rules, String> {
        let o = raw.as_object().ok_or("rules: expected an object")?;
        let version = o.get("version").and_then(Value::as_str).ok_or("rules: version missing")?;
        let g = |a: &str, b: &str| -> Result<i32, String> {
            int(o.get(a).and_then(|x| x.get(b)), &format!("{a}.{b}"))
        };

        let mut attacks = Vec::new();
        // serde_json's map iterates keys in sorted order, which is the order the indices use.
        let atk_obj = o.get("attacks").and_then(Value::as_object).ok_or("rules: attacks missing")?;
        for (name, a) in atk_obj {
            if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(format!("rules: attack name {name:?} is not supported by this port"));
            }
            if ["move", "dash", "shout"].contains(&name.as_str()) {
                return Err(format!("rules: attack name {name:?} collides with a cooldown key"));
            }
            let p = |k: &str| int(a.get(k), &format!("attacks.{name}.{k}"));
            let kind = match a.get("kind").and_then(Value::as_str) {
                Some("projectile") => {
                    let speed = p("speed")?;
                    if speed < 1 {
                        return Err(format!("rules: attacks.{name}.speed must be >= 1"));
                    }
                    let bounces = opt_int(a.get("bounces"), &format!("attacks.{name}.bounces"))?;
                    if let Some(b) = bounces {
                        if b != 1 {
                            return Err(format!("rules: attacks.{name}.bounces must be 1"));
                        }
                    }
                    AttackKind::Projectile {
                        range: opt_int(a.get("range"), &format!("attacks.{name}.range"))?,
                        speed,
                        bounces: bounces.unwrap_or(0),
                    }
                }
                Some("area") => {
                    let fuse = p("fuse")?;
                    if fuse < 3 {
                        return Err(format!("rules: attacks.{name}.fuse must be >= 3"));
                    }
                    let slow = opt_int(a.get("slow"), &format!("attacks.{name}.slow"))?;
                    if slow == Some(0) {
                        return Err(format!("rules: attacks.{name}.slow must be >= 1"));
                    }
                    AttackKind::Area {
                        range: p("range")?,
                        fuse,
                        radius: p("radius")?,
                        slow: slow.unwrap_or(0),
                    }
                }
                Some("burst") => {
                    let push = p("push")?;
                    if p("range")? != 1 || push < 1 {
                        return Err(format!(
                            "rules: attacks.{name} repulsor requires range 1 and push >= 1"
                        ));
                    }
                    AttackKind::Burst { push }
                }
                _ => return Err(format!("rules: attacks.{name}.kind invalid")),
            };
            attacks.push(Attack { name: name.clone(), damage: p("damage")?, cooldown: p("cooldown")?, kind });
        }
        if attacks.len() > MAX_ATTACKS {
            return Err(format!("rules: more than {MAX_ATTACKS} attacks is not supported by this port"));
        }

        let pickups = match o.get("pickups") {
            None | Some(Value::Null) => None,
            Some(p) => {
                let po = p.as_object().ok_or("rules: pickups must be an object")?;
                for k in po.keys() {
                    if !["first_spawn", "health", "power"].contains(&k.as_str()) {
                        return Err(format!("rules: pickups.{k} is not a known pickup kind"));
                    }
                }
                let health = match p.get("health") {
                    None => None,
                    Some(h) => {
                        let respawn = int(h.get("respawn"), "pickups.health.respawn")?;
                        if respawn < 1 {
                            return Err("rules: pickups.health.respawn must be >= 1".into());
                        }
                        Some(HealthRules { amount: int(h.get("amount"), "pickups.health.amount")?, respawn })
                    }
                };
                let power = match p.get("power") {
                    None => None,
                    Some(h) => {
                        let respawn = int(h.get("respawn"), "pickups.power.respawn")?;
                        let duration = int(h.get("duration"), "pickups.power.duration")?;
                        let m = h.get("damage_multiplier").and_then(Value::as_array);
                        let (n, d) = match m.map(|v| v.as_slice()) {
                            Some([n, d]) => (
                                int(Some(n), "pickups.power.damage_multiplier[0]")?,
                                int(Some(d), "pickups.power.damage_multiplier[1]")?,
                            ),
                            _ => return Err("rules: pickups.power.damage_multiplier invalid".into()),
                        };
                        if respawn < 1 || duration < 1 || d < 1 {
                            return Err("rules: pickups.power invalid".into());
                        }
                        Some(PowerRules { duration, mul_num: n, mul_den: d, respawn })
                    }
                };
                Some(PickupRules { first_spawn: int(p.get("first_spawn"), "pickups.first_spawn")?, health, power })
            }
        };

        let shout = match o.get("shout") {
            None | Some(Value::Null) => None,
            Some(s) => {
                let max_length = int(s.get("max_length"), "shout.max_length")?;
                if max_length < 1 {
                    return Err("rules: shout.max_length must be >= 1".into());
                }
                Some(ShoutRules { max_length, cooldown: int(s.get("cooldown"), "shout.cooldown")? })
            }
        };

        let zone = ZoneRules {
            start_tick: g("zone", "start_tick")?,
            interval: g("zone", "interval")?,
            step: g("zone", "step")?,
            min_radius: g("zone", "min_radius")?,
            warning_ticks: g("zone", "warning_ticks")?,
            damage_base: g("zone", "damage_base")?,
            damage_step: g("zone", "damage_step")?,
            damage_step_ticks: g("zone", "damage_step_ticks")?,
        };
        if zone.interval < 1 || zone.damage_step_ticks < 1 {
            return Err("rules: zone.interval and zone.damage_step_ticks must be >= 1".into());
        }

        let mut cooldown_keys: Vec<(String, CdSlot)> =
            vec![("move".to_string(), CdSlot::Move), ("dash".to_string(), CdSlot::Dash)];
        for (i, a) in attacks.iter().enumerate() {
            cooldown_keys.push((a.name.clone(), CdSlot::Attack(i as u8)));
        }
        if shout.is_some() {
            cooldown_keys.push(("shout".to_string(), CdSlot::Shout));
        }
        cooldown_keys.sort_by(|a, b| a.0.cmp(&b.0));

        Ok(Rules {
            version: version.to_string(),
            max_ticks: int(o.get("max_ticks"), "max_ticks")?,
            max_hp: g("stats", "max_hp")?,
            move_cooldown: g("stats", "move_cooldown")?,
            sight_radius: g("stats", "sight_radius")?,
            armor: g("stats", "armor")?,
            attacks,
            dash_distance: g("dash", "distance")?,
            dash_cooldown: g("dash", "cooldown")?,
            dash_recovery: g("dash", "recovery")?,
            wall_slam_damage: g("knockback", "wall_slam_damage")?,
            wall_slam_stun: g("knockback", "wall_slam_stun")?,
            fighter_collision_damage: g("knockback", "fighter_collision_damage")?,
            zone,
            pickups,
            shout,
            limits: Limits {
                startup_ms: g("limits", "startup_ms")?,
                tick_ms: g("limits", "tick_ms")?,
                match_budget_ms: g("limits", "match_budget_ms")?,
                max_consecutive_failures: g("limits", "max_consecutive_failures")?,
            },
            cooldown_keys,
            hash: hash_value(raw),
        })
    }

    pub fn attack_index(&self, name: &str) -> Option<u8> {
        self.attacks.iter().position(|a| a.name == name).map(|i| i as u8)
    }
}
