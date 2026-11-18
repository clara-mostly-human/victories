//! Grid geometry, ported from `packages/engine/src/geometry.ts`. Integer math only.
//!
//! Every function here mirrors one in the TypeScript source, including the order of
//! comparisons, so that results match the official engine exactly.

/// A tile or a vector, `[x, y]`.
pub type P = [i32; 2];

/// Directions clockwise from north, as indices 0..8.
pub const DIR_NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
pub const DIR_VEC: [P; 8] = [
    [0, -1],
    [1, -1],
    [1, 0],
    [1, 1],
    [0, 1],
    [-1, 1],
    [-1, 0],
    [-1, -1],
];

pub fn dir_from_name(s: &str) -> Option<u8> {
    DIR_NAMES.iter().position(|d| *d == s).map(|i| i as u8)
}

#[inline]
pub fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1]]
}

#[inline]
pub fn chebyshev(a: P, b: P) -> i32 {
    (a[0] - b[0]).abs().max((a[1] - b[1]).abs())
}

/// Octant of the vector from `from` to `to`; `None` when they are the same tile.
pub fn octant(from: P, to: P) -> Option<u8> {
    let dx = (to[0] - from[0]) as i64;
    let dy = (to[1] - from[1]) as i64;
    if dx == 0 && dy == 0 {
        return None;
    }
    let ax = dx.abs();
    let ay = dy.abs();
    const T_NUM: i64 = 41_421_356;
    const T_DEN: i64 = 100_000_000;
    if ay * T_DEN < ax * T_NUM {
        return Some(if dx > 0 { 2 } else { 6 });
    }
    if ax * T_DEN < ay * T_NUM {
        return Some(if dy > 0 { 4 } else { 0 });
    }
    Some(if dx > 0 {
        if dy > 0 {
            3
        } else {
            1
        }
    } else if dy > 0 {
        5
    } else {
        7
    })
}

// ---------------------------------------------------------------- projectile rays

/// Sub-tile units per tile.
pub const SUB: i64 = 1000;

#[derive(Clone, Copy, Debug)]
pub struct Ray {
    pub o: [i64; 2],
    /// target − origin; never [0, 0]
    pub d: [i64; 2],
    /// max(|dx|, |dy|)
    pub major: i64,
}

impl Ray {
    #[inline]
    pub fn new(origin: P, target: P) -> Ray {
        let d = [(target[0] - origin[0]) as i64, (target[1] - origin[1]) as i64];
        Ray { o: [origin[0] as i64, origin[1] as i64], d, major: d[0].abs().max(d[1].abs()) }
    }
}

/// a / b rounded half away from zero, b > 0.
#[inline]
fn round_div(a: i64, b: i64) -> i64 {
    let q = a / b; // truncates toward zero, like Math.trunc
    let r = a - q * b;
    if 2 * r.abs() >= b {
        q + a.signum()
    } else {
        q
    }
}

/// Point on the ray after travelling `s` SUB units, in SUB units.
#[inline]
pub fn ray_point(r: &Ray, s: i64) -> [i64; 2] {
    [r.o[0] * SUB + round_div(r.d[0] * s, r.major), r.o[1] * SUB + round_div(r.d[1] * s, r.major)]
}

/// Tile containing a SUB-unit point; points on a boundary belong to the higher tile.
#[inline]
pub fn sub_to_tile(p: [i64; 2]) -> [i64; 2] {
    [(p[0] + SUB / 2).div_euclid(SUB), (p[1] + SUB / 2).div_euclid(SUB)]
}

/// Rational n/d with d > 0.
#[derive(Clone, Copy, Debug)]
pub struct Q {
    pub n: i64,
    pub d: i64,
}

#[inline]
fn qlt(a: Q, b: Q) -> bool {
    a.n * b.d < b.n * a.d
}
#[inline]
fn qmax(a: Q, b: Q) -> Q {
    if qlt(a, b) {
        b
    } else {
        a
    }
}
#[inline]
fn qmin(a: Q, b: Q) -> Q {
    if qlt(a, b) {
        a
    } else {
        b
    }
}

impl Q {
    /// ceil(n / d)
    #[inline]
    pub fn ceil(self) -> i64 {
        let q = self.n.div_euclid(self.d);
        if self.n.rem_euclid(self.d) != 0 {
            q + 1
        } else {
            q
        }
    }
}

#[derive(Clone, Copy)]
struct Slab {
    lo: Option<Q>,
    hi: Option<Q>,
}

/// Open interval of travel during which the ray is strictly inside the slab of tile
/// coordinate `c` on one axis. `None` = never inside.
#[inline]
fn slab(o: i64, d_axis: i64, major: i64, c: i64) -> Option<Slab> {
    let l = (2 * (c - o) - 1) * SUB * major;
    let u = (2 * (c - o) + 1) * SUB * major;
    let k = 2 * d_axis;
    if k == 0 {
        return if l < 0 && 0 < u { Some(Slab { lo: None, hi: None }) } else { None };
    }
    Some(if k > 0 {
        Slab { lo: Some(Q { n: l, d: k }), hi: Some(Q { n: u, d: k }) }
    } else {
        Slab { lo: Some(Q { n: -u, d: -k }), hi: Some(Q { n: -l, d: -k }) }
    })
}

#[derive(Clone, Copy, Debug)]
pub struct RayTile {
    pub tile: P,
    /// travel (SUB units) at which the ray enters the tile, clamped to the segment start
    pub entry: Q,
}

/// Fixed-capacity output buffer for `ray_tiles`, so the hot path never allocates.
/// `N` bounds how many tiles one segment may cross: a projectile's one-tick sweep needs
/// few, a sight line across a large map needs more.
pub struct RayBuf<const N: usize> {
    pub len: usize,
    pub items: [RayTile; N],
}

/// Enough for one tick of any projectile up to speed 8.
pub type ShotBuf = RayBuf<40>;
/// Enough for a sight line across the largest map (41 tiles a side).
pub type SightBuf = RayBuf<192>;

impl<const N: usize> RayBuf<N> {
    pub fn new() -> Self {
        RayBuf { len: 0, items: [RayTile { tile: [0, 0], entry: Q { n: 0, d: 1 } }; N] }
    }
    #[inline]
    pub fn as_slice(&self) -> &[RayTile] {
        &self.items[..self.len]
    }
}

impl<const N: usize> Default for RayBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Tiles whose interior the ray passes through while travelling from `sa` to `sb`, in
/// order of entry (ties broken by y, then x).
///
/// The official engine tests every tile of a bounding box against both axis slabs. The
/// result here is the same set in the same order; the slab of each row is worked out
/// once instead of once per tile, which changes no comparison.
pub fn ray_tiles<const N: usize>(r: &Ray, sa: i64, sb: i64, out: &mut RayBuf<N>) {
    let a = ray_point(r, sa);
    let b = ray_point(r, sb);
    let t0 = sub_to_tile([a[0].min(b[0]), a[1].min(b[1])]);
    let t1 = sub_to_tile([a[0].max(b[0]), a[1].max(b[1])]);
    let start = Q { n: sa, d: 1 };
    let end = Q { n: sb, d: 1 };
    out.len = 0;
    const ROWS: usize = 48;
    let (y0, y1) = (t0[1] - 1, t1[1] + 1);
    let rows = (y1 - y0 + 1) as usize;
    let mut sy_cache: [Option<Slab>; ROWS] = [None; ROWS];
    let cached = rows <= ROWS;
    if cached {
        for (i, slot) in sy_cache[..rows].iter_mut().enumerate() {
            *slot = slab(r.o[1], r.d[1], r.major, y0 + i as i64);
        }
    }
    for x in (t0[0] - 1)..=(t1[0] + 1) {
        let sx = match slab(r.o[0], r.d[0], r.major, x) {
            Some(s) => s,
            None => continue,
        };
        for y in y0..=y1 {
            let sy = if cached { sy_cache[(y - y0) as usize] } else { slab(r.o[1], r.d[1], r.major, y) };
            let sy = match sy {
                Some(s) => s,
                None => continue,
            };
            let lo = match (sx.lo, sy.lo) {
                (Some(p), Some(q)) => Some(qmax(p, q)),
                (p, q) => p.or(q),
            };
            let hi = match (sx.hi, sy.hi) {
                (Some(p), Some(q)) => Some(qmin(p, q)),
                (p, q) => p.or(q),
            };
            if let Some(lo) = lo {
                if !qlt(lo, end) {
                    continue;
                }
            }
            if let Some(hi) = hi {
                if !qlt(start, hi) {
                    continue;
                }
            }
            if let (Some(lo), Some(hi)) = (lo, hi) {
                if !qlt(lo, hi) {
                    continue;
                }
            }
            let entry = match lo {
                Some(lo) if qlt(start, lo) => lo,
                _ => start,
            };
            assert!(out.len < N, "ray_tiles: buffer overflow");
            out.items[out.len] = RayTile { tile: [x as i32, y as i32], entry };
            out.len += 1;
        }
    }
    // Insertion sort by (entry, y, x). The comparator is a total order on distinct tiles,
    // so the result is the same as the engine's sort.
    let items = &mut out.items[..out.len];
    for i in 1..items.len() {
        let cur = items[i];
        let mut j = i;
        while j > 0 && tile_before(&cur, &items[j - 1]) {
            items[j] = items[j - 1];
            j -= 1;
        }
        items[j] = cur;
    }
}

#[inline]
fn tile_before(p: &RayTile, q: &RayTile) -> bool {
    if qlt(p.entry, q.entry) {
        return true;
    }
    if qlt(q.entry, p.entry) {
        return false;
    }
    if p.tile[1] != q.tile[1] {
        return p.tile[1] < q.tile[1];
    }
    p.tile[0] < q.tile[0]
}
