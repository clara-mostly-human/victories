//! Maps, ported from `packages/engine/src/map.ts` and `sight.ts`.
//!
//! Walls never change during a match, so everything that depends only on walls is worked
//! out once here: which steps are blocked from each tile, and line of sight between nearby
//! tiles. The tables are filled by the same ray code the engine uses, so they cannot drift
//! from it.

use crate::geom::{add, chebyshev, ray_tiles, Ray, SightBuf, DIR_VEC, P, SUB};
use serde_json::Value;
use std::sync::atomic::{AtomicU8, Ordering};

/// Most pickup pads a map may hold (shipped maps have four).
pub const MAX_PADS: usize = 8;

/// Line of sight is remembered for pairs of tiles at most this far apart (Chebyshev),
/// which covers the sight radius and every blast. Farther pairs are traced each time.
pub const LOS_RADIUS: i32 = 8;
const LOS_SPAN: usize = (2 * LOS_RADIUS + 1) as usize;
/// How far out `MapData::walls_near` looks: a 7 x 7 block is 49 tiles, one bit each.
pub const NEAR: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadKind {
    Health,
    Power,
}

impl PadKind {
    pub fn name(self) -> &'static str {
        match self {
            PadKind::Health => "health",
            PadKind::Power => "power",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PadDef {
    pub pos: P,
    pub kind: PadKind,
}

#[derive(Debug)]
pub struct MapData {
    pub name: String,
    pub w: i32,
    pub h: i32,
    walls: Vec<bool>,
    pub spawns: Vec<P>,
    pub pads: Vec<PadDef>,
    /// per tile, bit `d` set when a step in direction `d` is blocked
    blocked: Vec<u8>,
    /// per tile, a (2R+1)² block for the tiles around it: 0 not yet traced, 1 blocked,
    /// 2 clear. Filled on first use, like the official engine's cache; atomics keep the
    /// map shareable between search threads (a race only repeats a trace).
    los: Vec<AtomicU8>,
    /// per tile, the walls among the 7 x 7 tiles around it (`walls_near`)
    near: Vec<u64>,
    pub center: P,
}

impl MapData {
    pub fn from_json(raw: &Value) -> Result<MapData, String> {
        let name = raw.get("name").and_then(Value::as_str).ok_or("map: name missing")?;
        let tiles = raw.get("tiles").and_then(Value::as_array).ok_or("map: tiles missing")?;
        let h = tiles.len() as i32;
        let w = tiles.first().and_then(Value::as_str).map(|r| r.len()).unwrap_or(0) as i32;
        if !(11..=41).contains(&w) || !(11..=41).contains(&h) {
            return Err(format!("map: size {w}x{h} outside 11..41"));
        }
        let mut walls = Vec::with_capacity((w * h) as usize);
        for (y, row) in tiles.iter().enumerate() {
            let row = row.as_str().ok_or_else(|| format!("map: row {y} has wrong length"))?;
            if row.len() != w as usize {
                return Err(format!("map: row {y} has wrong length"));
            }
            for c in row.bytes() {
                match c {
                    b'.' => walls.push(false),
                    b'#' => walls.push(true),
                    _ => return Err(format!("map: bad tile '{}' in row {y}", c as char)),
                }
            }
        }
        let tile_of = |v: &Value| -> Option<P> {
            let a = v.as_array()?;
            Some([a.first()?.as_i64()? as i32, a.get(1)?.as_i64()? as i32])
        };
        let spawns: Vec<P> = raw
            .get("spawns")
            .and_then(Value::as_array)
            .ok_or("map: needs at least 2 spawns")?
            .iter()
            .map(|s| tile_of(s).ok_or("map: bad spawn"))
            .collect::<Result<_, _>>()?;
        if spawns.len() < 2 {
            return Err("map: needs at least 2 spawns".into());
        }
        let mut pads = Vec::new();
        if let Some(list) = raw.get("pickups") {
            for p in list.as_array().ok_or("map: pickups must be a list")? {
                let pos = p.get("pos").and_then(tile_of).ok_or("map: bad pickup pos")?;
                let kind = match p.get("kind").and_then(Value::as_str) {
                    Some("health") => PadKind::Health,
                    Some("power") => PadKind::Power,
                    other => return Err(format!("map: unknown pickup kind {other:?}")),
                };
                pads.push(PadDef { pos, kind });
            }
        }
        if pads.len() > MAX_PADS {
            return Err(format!("map: more than {MAX_PADS} pickup pads is not supported by this port"));
        }
        let mut m = MapData {
            name: name.to_string(),
            w,
            h,
            walls,
            spawns,
            pads,
            blocked: Vec::new(),
            los: Vec::new(),
            near: Vec::new(),
            center: [(w - 1) >> 1, (h - 1) >> 1],
        };
        for s in &m.spawns {
            if m.is_wall(*s) {
                return Err(format!("map: spawn {s:?} must be an in-bounds floor tile"));
            }
        }
        for p in &m.pads {
            if m.is_wall(p.pos) {
                return Err(format!("map: pickup {:?} must be an in-bounds floor tile", p.pos));
            }
        }
        m.build_tables();
        Ok(m)
    }

    /// The same map with different spawns (the ladder's `hello` sends tiles and pads only).
    pub fn with_spawns(mut self, spawns: Vec<P>) -> MapData {
        self.spawns = spawns;
        self
    }

    pub fn floor_tiles(&self) -> impl Iterator<Item = P> + '_ {
        (0..self.h).flat_map(move |y| (0..self.w).map(move |x| [x, y])).filter(move |p| !self.is_wall(*p))
    }

    fn build_tables(&mut self) {
        let n = (self.w * self.h) as usize;
        let mut blocked = vec![0u8; n];
        for y in 0..self.h {
            for x in 0..self.w {
                let mut bits = 0u8;
                for d in 0..8u8 {
                    if self.step_blocked_slow([x, y], d) {
                        bits |= 1 << d;
                    }
                }
                blocked[(y * self.w + x) as usize] = bits;
            }
        }
        self.blocked = blocked;

        self.los = (0..n * LOS_SPAN * LOS_SPAN).map(|_| AtomicU8::new(0)).collect();

        self.near = (0..n as i32).map(|i| self.walls_near_slow([i % self.w, i / self.w])).collect();
    }

    fn walls_near_slow(&self, p: P) -> u64 {
        let mut bits = 0u64;
        for dy in -NEAR..=NEAR {
            for dx in -NEAR..=NEAR {
                if self.is_wall([p[0] + dx, p[1] + dy]) {
                    bits |= 1 << ((dy + NEAR) * (2 * NEAR + 1) + dx + NEAR);
                }
            }
        }
        bits
    }

    /// The walls among the 7 x 7 tiles around `p`, one bit a tile, row by row from the top
    /// left: bit `(dy + 3) * 7 + dx + 3` is set when the tile `[dx, dy]` from `p` is a wall or
    /// off the board. Worked out once a tile when the map is read.
    #[inline]
    pub fn walls_near(&self, p: P) -> u64 {
        if self.in_bounds(p) {
            self.near[(p[1] * self.w + p[0]) as usize]
        } else {
            self.walls_near_slow(p)
        }
    }

    #[inline]
    pub fn in_bounds(&self, p: P) -> bool {
        p[0] >= 0 && p[1] >= 0 && p[0] < self.w && p[1] < self.h
    }

    /// Out-of-bounds reads as wall, which makes edges behave like walls everywhere.
    #[inline]
    pub fn is_wall(&self, p: P) -> bool {
        !self.in_bounds(p) || self.walls[(p[1] * self.w + p[0]) as usize]
    }

    fn step_blocked_slow(&self, from: P, dir: u8) -> bool {
        let [dx, dy] = DIR_VEC[dir as usize];
        if self.is_wall(add(from, [dx, dy])) {
            return true;
        }
        // A diagonal step is blocked if either orthogonal tile it cuts past is a wall.
        dx != 0 && dy != 0 && (self.is_wall([from[0] + dx, from[1]]) || self.is_wall([from[0], from[1] + dy]))
    }

    /// Can a fighter at `from` not step in `dir`: into a wall or off the map, or diagonally
    /// past a wall corner?
    #[inline]
    pub fn step_blocked(&self, from: P, dir: u8) -> bool {
        if self.in_bounds(from) {
            self.blocked[(from[1] * self.w + from[0]) as usize] & (1 << dir) != 0
        } else {
            self.step_blocked_slow(from, dir)
        }
    }

    /// The segment between two tile centres is blocked by any wall whose interior it
    /// crosses, the end tiles excepted.
    pub fn trace_sight(&self, a: P, b: P, buf: &mut SightBuf) -> bool {
        if a == b {
            return true;
        }
        let ray = Ray::new(a, b);
        ray_tiles(&ray, 0, ray.major * SUB, buf);
        for rt in buf.as_slice() {
            if rt.tile == a || rt.tile == b {
                continue;
            }
            if self.is_wall(rt.tile) {
                return false;
            }
        }
        true
    }

    pub fn has_line_of_sight(&self, a: P, b: P) -> bool {
        if a == b {
            return true;
        }
        if self.in_bounds(a) && self.in_bounds(b) && chebyshev(a, b) <= LOS_RADIUS {
            let base = (a[1] * self.w + a[0]) as usize * LOS_SPAN * LOS_SPAN;
            let i = base
                + ((b[1] - a[1] + LOS_RADIUS) as usize) * LOS_SPAN
                + (b[0] - a[0] + LOS_RADIUS) as usize;
            let known = self.los[i].load(Ordering::Relaxed);
            if known != 0 {
                return known == 2;
            }
            let mut buf = SightBuf::new();
            let clear = self.trace_sight(a, b, &mut buf);
            self.los[i].store(if clear { 2 } else { 1 }, Ordering::Relaxed);
            return clear;
        }
        let mut buf = SightBuf::new();
        self.trace_sight(a, b, &mut buf)
    }

    /// Is `p` inside the blast footprint centred on `pos`: within the square, not a wall,
    /// and in line of sight of the centre.
    #[inline]
    pub fn in_blast(&self, pos: P, radius: i32, p: P) -> bool {
        chebyshev(pos, p) <= radius && !self.is_wall(p) && self.has_line_of_sight(pos, p)
    }
}

impl Clone for MapData {
    fn clone(&self) -> MapData {
        MapData {
            name: self.name.clone(),
            w: self.w,
            h: self.h,
            walls: self.walls.clone(),
            spawns: self.spawns.clone(),
            pads: self.pads.clone(),
            blocked: self.blocked.clone(),
            los: self.los.iter().map(|x| AtomicU8::new(x.load(Ordering::Relaxed))).collect(),
            near: self.near.clone(),
            center: self.center,
        }
    }
}
