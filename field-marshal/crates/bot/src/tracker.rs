//! The state tracker: a full engine `State` kept in step with the server's match from
//! what one fighter is told each tick.
//!
//! The bot knows the whole starting position (`hello` carries the seed), its own turns, and
//! each tick's observation. The opponent's turn is not sent, but when the opponent is in
//! view its step, dash and attack can be worked out: the tracker tries each turn the
//! opponent could have played, runs the exact engine, and keeps the one whose predicted
//! observation equals the real one. That comparison is the live prediction check: any
//! tick where no candidate reproduces the observation is counted and logged.
//!
//! What stays hidden even in view is the opponent's step and weapon cooldowns; the tracker
//! infers them from what it saw the opponent do. Out of view, it keeps the set of tiles the
//! opponent could have reached.

use serde_json::{json, Value};
use victris_engine::geom::{add, chebyshev, DIR_VEC, P};
use victris_engine::observe::{as_tile, can_see, View};
use victris_engine::rules::AttackKind;
use victris_engine::state::{Area, FState, Pad, Projectile, State, EMPTY_AREA, EMPTY_PROJECTILE, MAX_AREAS, MAX_PROJECTILES, NO_BUFF};
use victris_engine::step::UNLIMITED_TRAVEL;
use victris_engine::turn::RawTurn;
use victris_engine::{shipped, Engine, MapData, Rules};

/// How one observation fitted the tracker's prediction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sync {
    /// first observation of a match (or after a gap): the belief was set up from it
    Start,
    /// an opponent turn was found that reproduces the observation exactly
    Exact,
    /// reproduced only after dropping an assumption about the opponent's hidden cooldowns
    Relaxed,
    /// the opponent came into view; its visible state was taken from the observation
    Reacquired,
    /// the opponent is out of view and the rest of the observation was predicted exactly
    Unseen,
    /// the opponent is out of view and something visible had to be corrected
    UnseenCorrected,
    /// the opponent was in view and no candidate reproduced the observation
    Mismatch,
}

impl Sync {
    pub fn name(&self) -> &'static str {
        match self {
            Sync::Start => "start",
            Sync::Exact => "exact",
            Sync::Relaxed => "relaxed",
            Sync::Reacquired => "reacquired",
            Sync::Unseen => "unseen",
            Sync::UnseenCorrected => "unseen_corrected",
            Sync::Mismatch => "mismatch",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrackStats {
    pub ticks: u32,
    pub exact: u32,
    pub relaxed: u32,
    pub reacquired: u32,
    pub unseen: u32,
    pub unseen_corrected: u32,
    pub mismatch: u32,
}

impl TrackStats {
    fn count(&mut self, s: &Sync) {
        self.ticks += 1;
        match s {
            Sync::Start => {}
            Sync::Exact => self.exact += 1,
            Sync::Relaxed => self.relaxed += 1,
            Sync::Reacquired => self.reacquired += 1,
            Sync::Unseen => self.unseen += 1,
            Sync::UnseenCorrected => self.unseen_corrected += 1,
            Sync::Mismatch => self.mismatch += 1,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({
            "ticks": self.ticks, "exact": self.exact, "relaxed": self.relaxed, "reacquired": self.reacquired,
            "unseen": self.unseen, "unseen_corrected": self.unseen_corrected, "mismatch": self.mismatch,
        })
    }
}

pub struct Tracker {
    pub engine: Engine,
    /// our fighter index (0 or 1)
    pub me: usize,
    pub seed: i64,
    spawns_known: bool,
    /// the state before the tick we are about to reply to; `None` until the first observation
    pub belief: Option<State>,
    /// is the opponent in view in the latest observation
    pub enemy_visible: bool,
    /// when and where the opponent was last seen
    pub enemy_last_seen: Option<(i32, P)>,
    /// tiles the opponent could be on, by tile index (`y * w + x`)
    pub possible: Vec<bool>,
    pub stats: TrackStats,
    /// which parts of the prediction were wrong at the last mismatch, for the log
    pub last_mismatch: Option<String>,
    /// damage dealt so far by each fighter, counted from events
    dealt: [i32; 2],
    /// dashes we have seen the opponent make in this match
    pub enemy_dashes: u32,
    /// our own estimate of match budget spent, in ms: think time plus a network allowance
    /// per tick. The server charges both; this keeps long thinks from exhausting the match.
    spent_ms: std::cell::Cell<u64>,
}

/// What a reply costs on top of thinking: network and server handling, as measured from
/// this machine (median 105 ms, 90th percentile 136 ms).
const TICK_OVERHEAD_MS: u64 = 125;
/// Budget kept in hand for the rest of a match.
const BUDGET_RESERVE_MS: u64 = 25_000;

/// Which parts of a predicted view differ from the real one, with both values.
fn view_diff(ours: &View, theirs: &View) -> Vec<String> {
    let mut out = Vec::new();
    macro_rules! part {
        ($name:literal, $field:ident) => {
            if ours.$field != theirs.$field {
                out.push(format!("{} predicted {:?} got {:?}", $name, ours.$field, theirs.$field));
            }
        };
    }
    part!("self", me);
    part!("enemy", enemy);
    part!("projectiles", projectiles);
    part!("areas", areas);
    part!("pickups", pickups);
    part!("moves", moves);
    part!("dashes", dashes);
    part!("zone", zone_radius);
    part!("next_shrink", next_shrink);
    if ours.danger != theirs.danger {
        out.push("danger differs".to_string());
    }
    out
}

/// Builds the engine for a match from `hello`.
pub fn engine_from_hello(hello: &Value) -> Result<(Engine, bool), String> {
    let rules = Rules::from_json(hello.get("rules").ok_or("hello: rules missing")?)?;
    let tiles = hello.get("map").ok_or("hello: map missing")?;
    let name = hello.get("map_name").and_then(Value::as_str).unwrap_or("unknown");
    let mut pads: Vec<&Value> = hello.get("pickups").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    pads.sort_by_key(|p| p.get("id").and_then(Value::as_i64).unwrap_or(0));
    let pickups: Vec<Value> = pads.iter().map(|p| json!({ "pos": p.get("pos"), "kind": p.get("kind") })).collect();

    // `hello` does not carry the spawns. For a shipped map they are known; otherwise use
    // placeholders and let the first observation set the fighters.
    let mut spawns: Option<Value> = None;
    for raw in shipped::MAPS_JSON {
        let m: Value = serde_json::from_str(raw).unwrap();
        if m.get("tiles") == Some(tiles) {
            spawns = m.get("spawns").cloned();
        }
    }
    let known = spawns.is_some();
    let probe = MapData::from_json(&json!({ "name": name, "tiles": tiles, "spawns": spawns.clone().unwrap_or(json!([[0, 0], [1, 0]])), "pickups": [] }));
    let spawns = match (spawns, probe) {
        (Some(s), _) => s,
        (None, Ok(m)) => {
            let two: Vec<P> = m.floor_tiles().take(2).collect();
            json!(two)
        }
        (None, Err(_)) => {
            // The placeholder spawns hit a wall: find floor tiles the slow way.
            let rows: Vec<&str> = tiles.as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            let mut two = Vec::new();
            for (y, row) in rows.iter().enumerate() {
                for (x, c) in row.bytes().enumerate() {
                    if c == b'.' && two.len() < 2 {
                        two.push([x as i32, y as i32]);
                    }
                }
            }
            json!(two)
        }
    };
    let map = MapData::from_json(&json!({ "name": name, "tiles": tiles, "spawns": spawns, "pickups": pickups }))?;
    Ok((Engine::new(rules, map)?, known))
}

impl Tracker {
    pub fn new(hello: &Value) -> Result<Tracker, String> {
        let (engine, spawns_known) = engine_from_hello(hello)?;
        let id = hello.get("bot_id").and_then(Value::as_i64).ok_or("hello: bot_id missing")?;
        if !(1..=2).contains(&id) {
            return Err(format!("hello: bot_id {id} is not 1 or 2"));
        }
        let n = (engine.map.w * engine.map.h) as usize;
        Ok(Tracker {
            engine,
            me: (id - 1) as usize,
            seed: hello.get("seed_public").and_then(Value::as_i64).unwrap_or(0),
            spawns_known,
            belief: None,
            enemy_visible: false,
            enemy_last_seen: None,
            possible: vec![false; n],
            stats: TrackStats::default(),
            last_mismatch: None,
            dealt: [0, 0],
            enemy_dashes: 0,
            spent_ms: std::cell::Cell::new(0),
        })
    }

    /// A tracker for a match run in-process (the arena), where the engine is already built.
    pub fn for_engine(engine: Engine, me: usize, seed: i64) -> Tracker {
        let n = (engine.map.w * engine.map.h) as usize;
        Tracker {
            engine,
            me,
            seed,
            spawns_known: true,
            belief: None,
            enemy_visible: false,
            enemy_last_seen: None,
            possible: vec![false; n],
            stats: TrackStats::default(),
            last_mismatch: None,
            dealt: [0, 0],
            enemy_dashes: 0,
            spent_ms: std::cell::Cell::new(0),
        }
    }

    #[inline]
    fn idx(&self, p: P) -> usize {
        (p[1] * self.engine.map.w + p[0]) as usize
    }

    /// Records one decision's think time against the match budget.
    pub fn charge_think(&self, took: std::time::Duration) {
        self.spent_ms.set(self.spent_ms.get() + took.as_millis() as u64 + TICK_OVERHEAD_MS);
    }

    /// The think cap for this tick: the playbook's, cut down when the match budget could
    /// not sustain it for the ticks that may remain.
    pub fn think_cap_ms(&self, wanted: u64) -> u64 {
        let rules = &self.engine.rules;
        let budget = rules.limits.match_budget_ms.max(0) as u64;
        let left = budget.saturating_sub(self.spent_ms.get()).saturating_sub(BUDGET_RESERVE_MS);
        let tick = self.belief.map(|s| s.tick).unwrap_or(0).max(0);
        let ticks_left = (rules.max_ticks - tick).max(1) as u64;
        let sustainable = (left / ticks_left).saturating_sub(TICK_OVERHEAD_MS);
        // Most games end long before the tick limit, so allow double the sustainable rate,
        // but never more than the tick deadline can carry.
        let per_tick_limit = (rules.limits.tick_ms.max(0) as u64).saturating_sub(400);
        wanted.min(sustainable * 2).min(per_tick_limit)
    }

    pub fn my_id(&self) -> i64 {
        self.me as i64 + 1
    }
    pub fn enemy_id(&self) -> i64 {
        2 - self.me as i64
    }

    /// Takes in the observation for a tick. `my_prev` is the turn we sent for the previous
    /// tick, `events` the observation's event list. Returns how well it was predicted.
    pub fn observe(&mut self, view: &View, events: &[Value], my_prev: Option<RawTurn>) -> Sync {
        let sync = self.observe_inner(view, events, my_prev);
        self.stats.count(&sync);
        let their_id = self.enemy_id();
        self.enemy_dashes += events
            .iter()
            .filter(|e| e.get("kind").and_then(Value::as_str) == Some("dash") && e.get("fighter").and_then(Value::as_i64) == Some(their_id))
            .count() as u32;
        // Every hit involves us as attacker or target, so we are told about all of them:
        // the damage each fighter has dealt is known exactly, whatever was out of view.
        if sync == Sync::Start {
            if let Some(s) = &self.belief {
                self.dealt = [s.fighters[0].damage_dealt, s.fighters[1].damage_dealt];
            }
        } else {
            for e in events {
                let kind = e.get("kind").and_then(Value::as_str).unwrap_or("");
                if kind == "hit" || kind == "wall_slam" {
                    let a = e.get("attacker").and_then(Value::as_i64).unwrap_or(0);
                    let d = e.get("damage").and_then(Value::as_i64).unwrap_or(0) as i32;
                    if (1..=2).contains(&a) && e.get("target").and_then(Value::as_i64) != Some(a) {
                        self.dealt[(a - 1) as usize] += d;
                    }
                }
            }
            if let Some(s) = self.belief.as_mut() {
                s.fighters[0].damage_dealt = self.dealt[0];
                s.fighters[1].damage_dealt = self.dealt[1];
            }
        }
        self.enemy_visible = view.enemy.is_some();
        if let Some(e) = &view.enemy {
            self.enemy_last_seen = Some((view.tick, e.pos));
        }
        self.update_possible(view, events);
        sync
    }

    fn observe_inner(&mut self, view: &View, events: &[Value], my_prev: Option<RawTurn>) -> Sync {
        let me = self.me;
        let prev = match self.belief {
            Some(s) if s.tick + 1 == view.tick && s.result.is_none() => s,
            Some(s) if s.tick == view.tick => {
                // The same tick again (a resend after a reconnect): nothing new to learn.
                let _ = s;
                return Sync::Start;
            }
            _ => {
                self.start_from(view);
                return Sync::Start;
            }
        };

        // What the server actually played for us: our turn, or idle if it refused the message.
        let refused = events.iter().any(|e| {
            e.get("kind").and_then(Value::as_str) == Some("rejected")
                && e.get("fighter").and_then(Value::as_i64) == Some(self.my_id())
                && e.get("part").and_then(Value::as_str) == Some("(message)")
        });
        let mine = if refused { RawTurn::IDLE } else { my_prev.unwrap_or(RawTurn::IDLE) };

        // What the opponent was seen doing.
        let enemy_event = |kind: &str| {
            events.iter().find(|e| {
                e.get("kind").and_then(Value::as_str) == Some(kind) && e.get("fighter").and_then(Value::as_i64) == Some(self.enemy_id())
            })
        };
        let attack = enemy_event("attack").and_then(|e| {
            let a = self.engine.rules.attack_index(e.get("attack")?.as_str()?)?;
            Some((a, as_tile(e.get("target"))?))
        });
        let dashed = enemy_event("dash").is_some();
        let with_attack = |t: RawTurn| match attack {
            Some((a, target)) => {
                if self.engine.rules.attacks[a as usize].is_burst() {
                    t.with_burst(a)
                } else {
                    t.with_attack(a, target)
                }
            }
            None => t,
        };
        let mut candidates: Vec<RawTurn> = Vec::with_capacity(9);
        if dashed {
            candidates.extend((0..8u8).map(|d| with_attack(RawTurn::dash(d))));
        } else {
            candidates.push(with_attack(RawTurn::IDLE));
            candidates.extend((0..8u8).map(|d| with_attack(RawTurn::step(d))));
        }
        // Shots and areas share one id counter, and an opponent firing out of view moves it
        // without our knowing. Any spawn we are told about this tick says where it stood.
        let mut prev = prev;
        let first_new = events
            .iter()
            .filter_map(|e| match e.get("kind").and_then(Value::as_str) {
                Some("projectile_spawn") => e.get("projectile").and_then(Value::as_u64),
                Some("area_spawn") => e.get("area").and_then(Value::as_u64),
                _ => None,
            })
            .min();
        if let Some(id) = first_new {
            prev.next_projectile_id = id as u32;
        }
        // A pad now in view that was emptied before last tick (out of our sight) was already
        // empty when the tick began. Knowing that matters: otherwise a fighter stepping onto
        // it would be predicted to pick something up.
        if let Some(pr) = self.engine.rules.pickups.as_ref() {
            for p in &view.pickups {
                let respawn = match p.kind {
                    victris_engine::map::PadKind::Health => pr.health.map(|h| h.respawn),
                    victris_engine::map::PadKind::Power => pr.power.map(|h| h.respawn),
                };
                let taken_last_tick = respawn.map(|r| p.ready_in == r - 1).unwrap_or(false);
                if p.ready_in > 0 && !taken_last_tick {
                    let pad = &mut prev.pads[(p.id - 1) as usize];
                    pad.available = false;
                    pad.respawn_tick = view.tick + p.ready_in;
                }
            }
        }
        let turns = |theirs: RawTurn| if me == 0 { [mine, theirs] } else { [theirs, mine] };
        // Some of what an observation shows could not have been predicted, because it was
        // decided out of our sight: an opponent's shot or area flying into view, and the
        // state of a pad we could not see a tick ago. Those are taken from the observation
        // and are not prediction errors; everything else must match exactly.
        let knew_shot = |id: u32| prev.projectiles().iter().any(|p| p.id == id);
        let knew_area = |id: u32| prev.areas().iter().any(|b| b.id == id);
        let enemy_no = (2 - me) as u8;
        let news = view.projectiles.iter().any(|p| p.owner == enemy_no && !knew_shot(p.id))
            || view.areas.iter().any(|b| b.owner == enemy_no && !knew_area(b.id));
        let was_seen = |pos: P| can_see(&self.engine.rules, &self.engine.map, prev.fighters[me].pos, pos);
        let fits = |ours: &View| -> bool {
            if ours == view {
                return true;
            }
            ours.tick == view.tick
                && ours.me == view.me
                && ours.enemy == view.enemy
                && ours.moves == view.moves
                && ours.dashes == view.dashes
                && ours.zone_radius == view.zone_radius
                && ours.next_shrink == view.next_shrink
                // Everything we predicted must be there as predicted; anything extra must be
                // an opponent's shot or area we had no way of knowing about.
                && ours.projectiles.iter().all(|p| view.projectiles.contains(p))
                && view.projectiles.iter().all(|p| ours.projectiles.contains(p) || (p.owner == enemy_no && !knew_shot(p.id)))
                && ours.areas.iter().all(|b| view.areas.contains(b))
                && view.areas.iter().all(|b| ours.areas.contains(b) || (b.owner == enemy_no && !knew_area(b.id)))
                && ours.pickups.len() == view.pickups.len()
                && ours.pickups.iter().zip(&view.pickups).all(|(a, b)| a.id == b.id && (a == b || !was_seen(b.pos)))
                && (news || ours.danger == view.danger)
        };
        let try_all = |from: &State| -> Option<State> {
            for c in &candidates {
                let mut s = *from;
                self.engine.step(&mut s, &turns(*c));
                if s.result.is_some() {
                    continue;
                }
                let ours = View::from_state(&self.engine, &s, me);
                if fits(&ours) {
                    if ours != *view {
                        self.overlay(&mut s, view);
                    }
                    return Some(s);
                }
            }
            None
        };

        if view.enemy.is_some() && self.enemy_visible {
            if let Some(s) = try_all(&prev) {
                self.belief = Some(s);
                return Sync::Exact;
            }
            // The opponent did something our belief said it could not: its hidden cooldowns
            // were wrong. Assume everything it has was ready and try again.
            let mut relaxed = prev;
            let o = &mut relaxed.fighters[1 - me];
            o.cd_move = 0;
            if let Some((a, _)) = attack {
                o.cd_attack[a as usize] = 0;
            }
            if let Some(s) = try_all(&relaxed) {
                self.belief = Some(s);
                return Sync::Relaxed;
            }
            // Nothing fits. Record which parts of the closest prediction were wrong, then
            // take what the observation shows.
            let mut closest: Option<(usize, String, State)> = None;
            for c in &candidates {
                let mut s = relaxed;
                self.engine.step(&mut s, &turns(*c));
                let parts = view_diff(&View::from_state(&self.engine, &s, me), view);
                if closest.as_ref().map(|(n, _, _)| parts.len() < *n).unwrap_or(true) {
                    closest = Some((parts.len(), parts.join("; "), s));
                }
            }
            let (_, why, mut s) = closest.unwrap();
            self.last_mismatch = Some(format!(
                "{why}; before: me at {:?} (state {:?}), opponent at {:?}, sent {:?}",
                prev.fighters[me].pos, prev.fighters[me].state, prev.fighters[1 - me].pos, mine
            ));
            self.overlay(&mut s, view);
            self.belief = Some(s);
            return Sync::Mismatch;
        }

        if view.enemy.is_some() {
            // The opponent came into view. Where it was is a guess, so take what is visible
            // from the observation and assume its step is ready.
            let mut s = prev;
            self.engine.step(&mut s, &turns(RawTurn::IDLE));
            self.overlay(&mut s, view);
            let o = &mut s.fighters[1 - me];
            o.cd_move = 0;
            if let Some((a, _)) = attack {
                o.cd_attack[a as usize] = (self.engine.rules.attacks[a as usize].cooldown - 1).max(0);
            }
            self.belief = Some(s);
            return Sync::Reacquired;
        }

        // The opponent is out of view. If it was in view a tick ago, one of its moves
        // explains where it went; otherwise leave the ghost where it is.
        let found = if self.enemy_visible { try_all(&prev) } else { None };
        let mut s = match found {
            Some(s) => {
                self.belief = Some(s);
                return Sync::Unseen;
            }
            None => {
                let mut s = prev;
                self.engine.step(&mut s, &turns(RawTurn::IDLE));
                s
            }
        };
        // Hits we landed on a hidden opponent are still reported to us.
        let hits: i32 = events
            .iter()
            .filter(|e| {
                e.get("kind").and_then(Value::as_str) == Some("hit") && e.get("target").and_then(Value::as_i64) == Some(self.enemy_id())
            })
            .filter_map(|e| e.get("damage").and_then(Value::as_i64))
            .sum::<i64>() as i32;
        let exact = s.result.is_none() && fits(&View::from_state(&self.engine, &s, me));
        if exact {
            self.overlay(&mut s, view);
        }
        if !exact {
            self.overlay(&mut s, view);
        }
        if hits > 0 {
            let o = &mut s.fighters[1 - me];
            o.hp = o.hp.min(prev.fighters[1 - me].hp - hits).max(1);
        }
        self.belief = Some(s);
        if exact {
            Sync::Unseen
        } else {
            Sync::UnseenCorrected
        }
    }

    /// Sets the belief up from one observation, with no history.
    fn start_from(&mut self, view: &View) {
        let e = &self.engine;
        let me = self.me;
        if view.tick == 0 && self.spawns_known {
            let s = e.new_match(self.seed);
            if View::from_state(e, &s, me) == *view {
                self.belief = Some(s);
                return;
            }
        }
        // A match already under way, or a map we have no spawns for: start from the
        // observation and place the unseen opponent opposite us.
        let mut s = e.new_match(self.seed);
        s.tick = view.tick;
        let first_spawn = e.rules.pickups.as_ref().map(|p| p.first_spawn).unwrap_or(0);
        for pad in s.pads.iter_mut() {
            *pad = Pad { available: view.tick >= first_spawn, respawn_tick: first_spawn };
        }
        let mirror = [e.map.w - 1 - view.me.pos[0], e.map.h - 1 - view.me.pos[1]];
        s.fighters[1 - me].pos = if !e.map.is_wall(mirror) && mirror != view.me.pos {
            mirror
        } else {
            e.map.floor_tiles().find(|p| *p != view.me.pos && !can_see(&e.rules, &e.map, view.me.pos, *p)).unwrap_or(mirror)
        };
        self.overlay(&mut s, view);
        self.belief = Some(s);
    }

    /// Overwrites everything the observation shows. What it does not show is kept.
    fn overlay(&self, s: &mut State, v: &View) {
        let e = &self.engine;
        let me = self.me;
        s.tick = v.tick;
        s.result = None;
        s.zone_radius = v.zone_radius;
        {
            let f = &mut s.fighters[me];
            f.pos = v.me.pos;
            f.hp = v.me.hp;
            f.alive = true;
            f.invulnerable = false;
            f.state = v.me.state;
            if matches!(v.me.state, FState::Recovering | FState::Stunned) {
                f.state_end = v.tick + v.me.state_ticks_left - 1;
            }
            f.cd_move = v.me.cd_move;
            f.cd_dash = v.me.cd_dash;
            f.cd_shout = v.me.cd_shout;
            f.cd_attack = v.me.cd_attack;
            f.power_until = v.me.power_ticks_left.map(|l| v.tick + l - 1).unwrap_or(NO_BUFF);
        }
        {
            let o = &mut s.fighters[1 - me];
            o.alive = true;
            o.invulnerable = false;
            if o.state == FState::Dead {
                o.state = FState::Idle;
            }
            if let Some(x) = &v.enemy {
                o.pos = x.pos;
                o.hp = x.hp;
                o.cd_dash = x.dash_cooldown;
                o.state = x.state;
                if matches!(x.state, FState::Recovering | FState::Stunned) {
                    o.state_end = v.tick + x.state_ticks_left - 1;
                }
                o.power_until = x.power_ticks_left.map(|l| v.tick + l - 1).unwrap_or(NO_BUFF);
                // Time spent outside the zone is not shown, but standing inside resets it:
                // the last tick judged it from this tile against the radius then in force.
                let radius_then = victris_engine::step::zone_radius_at(&e.rules, &e.map, v.tick - 2);
                if chebyshev(x.pos, e.map.center) <= radius_then {
                    o.ticks_outside = 0;
                } else {
                    o.ticks_outside = o.ticks_outside.max(1);
                }
            } else if o.pos == v.me.pos {
                // A ghost cannot share our tile.
                if let Some(p) = e.map.floor_tiles().find(|p| !can_see(&e.rules, &e.map, v.me.pos, *p)) {
                    o.pos = p;
                }
            }
        }

        // Projectiles: everything in view, plus the opponent's shots we believed in that are out of view.
        let mut shots: Vec<Projectile> = Vec::new();
        for p in &v.projectiles {
            let known = s.projectiles().iter().find(|q| q.id == p.id).copied();
            let dx = (p.target[0] - p.origin[0]).abs();
            let dy = (p.target[1] - p.origin[1]).abs();
            let axis = if dx >= dy { 0 } else { 1 };
            let traveled = (p.exact_pos[axis] - p.origin[axis] * 1000).abs();
            let max_travel = match known {
                Some(q) if q.origin == p.origin && q.target == p.target => q.max_travel,
                _ => match e.rules.attacks[p.attack as usize].kind {
                    AttackKind::Projectile { range: Some(r), .. } => r * 1000,
                    _ => UNLIMITED_TRAVEL as i32,
                },
            };
            shots.push(Projectile {
                id: p.id,
                owner: p.owner - 1,
                attack: p.attack,
                origin: p.origin,
                target: p.target,
                traveled,
                max_travel,
                pos: p.pos,
                exact_pos: p.exact_pos,
                speed: p.speed,
                damage: p.damage,
                bounces_left: p.bounces_left,
            });
        }
        for q in s.projectiles() {
            let hidden = q.owner as usize != me && !can_see(&e.rules, &e.map, v.me.pos, q.pos);
            if hidden && !shots.iter().any(|p| p.id == q.id) {
                shots.push(*q);
            }
        }
        shots.sort_by_key(|p| p.id);
        shots.truncate(MAX_PROJECTILES);
        s.projectiles = [EMPTY_PROJECTILE; MAX_PROJECTILES];
        for (i, p) in shots.iter().enumerate() {
            s.projectiles[i] = *p;
        }
        s.n_projectiles = shots.len() as u8;

        let mut areas: Vec<Area> = Vec::new();
        for b in &v.areas {
            let known = s.areas().iter().find(|q| q.id == b.id).copied();
            let explodes_tick = v.tick + b.ticks_left - 1;
            let fuse = match e.rules.attacks[b.attack as usize].kind {
                AttackKind::Area { fuse, .. } => fuse,
                _ => 0,
            };
            areas.push(Area {
                id: b.id,
                owner: b.owner - 1,
                attack: b.attack,
                pos: b.pos,
                origin: known.map(|q| q.origin).unwrap_or(s.fighters[(b.owner - 1) as usize].pos),
                launched_tick: known.map(|q| q.launched_tick).unwrap_or(explodes_tick - fuse),
                explodes_tick,
                radius: b.radius,
                damage: b.damage,
                slow: b.slow,
            });
        }
        for q in s.areas() {
            let hidden = q.owner as usize != me && !can_see(&e.rules, &e.map, v.me.pos, q.pos);
            if hidden && !areas.iter().any(|b| b.id == q.id) {
                areas.push(*q);
            }
        }
        areas.sort_by_key(|b| b.id);
        areas.truncate(MAX_AREAS);
        s.areas = [EMPTY_AREA; MAX_AREAS];
        for (i, b) in areas.iter().enumerate() {
            s.areas[i] = *b;
        }
        s.n_areas = areas.len() as u8;

        for p in &v.pickups {
            let pad = &mut s.pads[(p.id - 1) as usize];
            if p.ready_in == 0 {
                pad.available = true;
                pad.respawn_tick = pad.respawn_tick.min(v.tick);
            } else {
                pad.available = false;
                pad.respawn_tick = v.tick + p.ready_in;
            }
        }
        let top = v.projectiles.iter().map(|p| p.id).chain(v.areas.iter().map(|b| b.id)).max().unwrap_or(0);
        s.next_projectile_id = s.next_projectile_id.max(top + 1);
    }

    /// Keeps the set of tiles the opponent could be on, and moves the belief's ghost into it.
    fn update_possible(&mut self, view: &View, events: &[Value]) {
        let n = self.possible.len();
        let (w, h) = (self.engine.map.w, self.engine.map.h);
        if let Some(x) = &view.enemy {
            self.possible.iter_mut().for_each(|b| *b = false);
            let i = self.idx(x.pos);
            self.possible[i] = true;
            return;
        }
        let Some(mut s) = self.belief else { return };
        let me = self.me;
        if !self.possible.iter().any(|b| *b) {
            // No history: anywhere we cannot see.
            for p in self.engine.map.floor_tiles().collect::<Vec<_>>() {
                let i = self.idx(p);
                self.possible[i] = true;
            }
        } else {
            // One tick of movement: a step, or a dash if it may be ready.
            let reach = if s.fighters[1 - me].cd_dash <= 1 { self.engine.rules.dash_distance.max(1) } else { 1 };
            for _ in 0..reach {
                let mut next = self.possible.clone();
                for i in 0..n {
                    if !self.possible[i] {
                        continue;
                    }
                    let p = [(i as i32) % w, (i as i32) / w];
                    for d in 0..8u8 {
                        if !self.engine.map.step_blocked(p, d) {
                            let q = add(p, DIR_VEC[d as usize]);
                            next[(q[1] * w + q[0]) as usize] = true;
                        }
                    }
                }
                self.possible = next;
            }
        }
        // A hit of ours that ended on a hidden opponent says where it stood.
        let hit_at = events.iter().find_map(|e| {
            if e.get("kind").and_then(Value::as_str) == Some("projectile_end") && e.get("reason").and_then(Value::as_str) == Some("hit") {
                as_tile(e.get("pos")).filter(|p| *p != view.me.pos && p[0] >= 0 && p[1] >= 0 && p[0] < w && p[1] < h)
            } else {
                None
            }
        });
        // It is not on any tile we can see, nor on ours.
        for i in 0..n {
            if self.possible[i] {
                let p = [(i as i32) % w, (i as i32) / w];
                if p == view.me.pos || can_see(&self.engine.rules, &self.engine.map, view.me.pos, p) {
                    self.possible[i] = false;
                }
            }
        }
        if let Some(p) = hit_at {
            if !can_see(&self.engine.rules, &self.engine.map, view.me.pos, p) {
                self.possible.iter_mut().for_each(|b| *b = false);
                let i = self.idx(p);
                self.possible[i] = true;
            }
        }
        if !self.possible.iter().any(|b| *b) {
            for p in self.engine.map.floor_tiles().collect::<Vec<_>>() {
                if p != view.me.pos && !can_see(&self.engine.rules, &self.engine.map, view.me.pos, p) {
                    let i = self.idx(p);
                    self.possible[i] = true;
                }
            }
        }
        // Keep the ghost on a possible tile: the nearest one to where we believed it was.
        let ghost = s.fighters[1 - me].pos;
        if !self.possible.get(self.idx(ghost)).copied().unwrap_or(false) {
            let mut best: Option<(i32, P)> = None;
            for i in 0..n {
                if self.possible[i] {
                    let p = [(i as i32) % w, (i as i32) / w];
                    let d = chebyshev(p, ghost);
                    if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                        best = Some((d, p));
                    }
                }
            }
            if let Some((_, p)) = best {
                s.fighters[1 - me].pos = p;
                self.belief = Some(s);
            }
        }
    }
}
