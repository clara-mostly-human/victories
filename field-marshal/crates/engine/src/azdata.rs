//! Self-play games as `azgen` saves them and `aztrain` reads them: the seed, every tick's pair
//! of turns, and for every searched tick where the search put its visits and what it thought
//! the position was worth. The trainer replays the game on the engine to get the positions
//! back, so what the network is shown of a position can change without playing the games again.
//!
//! A tick can also carry positions beside the game's path (`SideLog`): a pair of turns the
//! search tried from the tick's position but the game did not play, and what a closer look at
//! the position after it found. The network's value is asked about such positions all the
//! time (they are where the search's lines end), yet a game only ever shows it the positions
//! that good play reached; these teach it what the others are worth.

use crate::pvnet::{class_of, Sample, CLASSES};
use crate::turn::{AttackIn, DirIn, TargetIn};
use crate::{Engine, RawTurn};

const MAGIC: &[u8; 4] = b"AZG2";
/// A game with positions beside its path. Programs built before those stop reading at it.
const MAGIC_SIDE: &[u8; 4] = b"AZG3";

/// A position beside the game's path: the pair of turns that leads to it from the tick's
/// position, the search's value of it for fighter 0 (-1 to 1) and the lines behind that value.
#[derive(Clone, Debug, PartialEq)]
pub struct SideLog {
    pub turns: [RawTurn; 2],
    pub q: f32,
    pub lines: u32,
}

#[derive(Clone)]
pub struct TickLog {
    pub turns: [RawTurn; 2],
    /// the search's value for fighter 0, when it counts in wins (-1 to 1)
    pub q: Option<f32>,
    /// per fighter, the share of visits on each kind of turn (out of 255); `None` when the tick
    /// was not searched (neither fighter could act)
    pub pol: Option<[[u8; CLASSES]; 2]>,
    /// positions beside the game's path, reached from this tick's position (at most 255)
    pub side: Vec<SideLog>,
}

#[derive(Clone)]
pub struct GameLog {
    pub seed: i64,
    /// 1 fighter 0 won, -1 fighter 1 won, 0 a draw
    pub z: i8,
    pub ticks: Vec<TickLog>,
}

/// Earned visits (`Search::root_earned`) by kind of turn, as shares out of 255. A kind that
/// earned less than half a visit is left out.
pub fn shares_earned(visits: &[(RawTurn, f32)]) -> [u8; CLASSES] {
    let mut by = [0.0f32; CLASSES];
    for (t, n) in visits {
        by[class_of(t)] += *n;
    }
    let total: f32 = by.iter().filter(|v| **v >= 0.5).sum();
    let mut out = [0u8; CLASSES];
    for c in 0..CLASSES {
        if by[c] >= 0.5 && total > 0.0 {
            out[c] = ((by[c] as f64 / total as f64) * 255.0).round().max(1.0) as u8;
        }
    }
    out
}

/// Visits by kind of turn, as shares out of 255 (a visited kind never rounds down to zero).
pub fn shares(visits: &[(RawTurn, u32)]) -> [u8; CLASSES] {
    let mut by = [0u32; CLASSES];
    for (t, n) in visits {
        by[class_of(t)] += *n;
    }
    let total: u32 = by.iter().sum();
    let mut out = [0u8; CLASSES];
    for c in 0..CLASSES {
        if by[c] > 0 {
            out[c] = ((by[c] as f64 / total as f64) * 255.0).round().max(1.0) as u8;
        }
    }
    out
}

fn put_turn(t: &RawTurn, out: &mut Vec<u8>) {
    let dir = |d: DirIn| match d {
        DirIn::Dir(d) => d,
        _ => 255,
    };
    out.push(dir(t.mv));
    out.push(dir(t.dash));
    match t.attack {
        AttackIn::Named { attack, target: TargetIn::Tile(p) } => out.extend_from_slice(&[attack, 1, p[0] as i8 as u8, p[1] as i8 as u8]),
        AttackIn::Named { attack, .. } => out.extend_from_slice(&[attack, 0, 0, 0]),
        _ => out.extend_from_slice(&[255, 0, 0, 0]),
    }
}

fn get_turn(b: &[u8]) -> RawTurn {
    let dir = |d: u8| if d < 8 { DirIn::Dir(d) } else { DirIn::None };
    let attack = match (b[2], b[3]) {
        (255, _) => AttackIn::None,
        (a, 1) => AttackIn::Named { attack: a, target: TargetIn::Tile([b[4] as i8 as i32, b[5] as i8 as i32]) },
        (a, _) => AttackIn::Named { attack: a, target: TargetIn::Missing },
    };
    RawTurn { mv: dir(b[0]), dash: dir(b[1]), attack, ..RawTurn::IDLE }
}

impl GameLog {
    pub fn write(&self, out: &mut Vec<u8>) {
        let with_side = self.ticks.iter().any(|t| !t.side.is_empty());
        out.extend_from_slice(if with_side { MAGIC_SIDE } else { MAGIC });
        out.extend_from_slice(&self.seed.to_le_bytes());
        out.push(self.z as u8);
        out.extend_from_slice(&(self.ticks.len() as u32).to_le_bytes());
        for t in &self.ticks {
            put_turn(&t.turns[0], out);
            put_turn(&t.turns[1], out);
            let side = &t.side[..t.side.len().min(255)];
            out.push(t.pol.is_some() as u8 | (t.q.is_some() as u8) << 1 | (!side.is_empty() as u8) << 2);
            out.push((t.q.unwrap_or(0.0).clamp(-1.0, 1.0) * 127.0).round() as i8 as u8);
            if let Some(pol) = &t.pol {
                for f in 0..2 {
                    let n = pol[f].iter().filter(|v| **v > 0).count();
                    out.push(n as u8);
                    for c in 0..CLASSES {
                        if pol[f][c] > 0 {
                            out.push(c as u8);
                            out.push(pol[f][c]);
                        }
                    }
                }
            }
            if !side.is_empty() {
                out.push(side.len() as u8);
                for k in side {
                    put_turn(&k.turns[0], out);
                    put_turn(&k.turns[1], out);
                    out.push((k.q.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8);
                    out.extend_from_slice(&(k.lines.min(u16::MAX as u32) as u16).to_le_bytes());
                }
            }
        }
    }

    /// Every game in a file's bytes. Stops quietly at a cut-off game.
    pub fn read_all(b: &[u8]) -> Vec<GameLog> {
        let mut games = Vec::new();
        let mut at = 0usize;
        'games: while at + 17 <= b.len() && (&b[at..at + 4] == MAGIC || &b[at..at + 4] == MAGIC_SIDE) {
            let seed = i64::from_le_bytes(b[at + 4..at + 12].try_into().unwrap());
            let z = b[at + 12] as i8;
            let n = u32::from_le_bytes(b[at + 13..at + 17].try_into().unwrap()) as usize;
            at += 17;
            let mut ticks = Vec::with_capacity(n);
            for _ in 0..n {
                if at + 14 > b.len() {
                    break 'games;
                }
                let turns = [get_turn(&b[at..at + 6]), get_turn(&b[at + 6..at + 12])];
                let (flags, q) = (b[at + 12], b[at + 13] as i8 as f32 / 127.0);
                at += 14;
                let mut pol = None;
                if flags & 1 != 0 {
                    let mut p = [[0u8; CLASSES]; 2];
                    for f in 0..2 {
                        if at >= b.len() {
                            break 'games;
                        }
                        let k = b[at] as usize;
                        at += 1;
                        if at + 2 * k > b.len() {
                            break 'games;
                        }
                        for i in 0..k {
                            let c = b[at + 2 * i] as usize;
                            if c < CLASSES {
                                p[f][c] = b[at + 2 * i + 1];
                            }
                        }
                        at += 2 * k;
                    }
                    pol = Some(p);
                }
                let mut side = Vec::new();
                if flags & 4 != 0 {
                    if at >= b.len() {
                        break 'games;
                    }
                    let k = b[at] as usize;
                    at += 1;
                    if at + 15 * k > b.len() {
                        break 'games;
                    }
                    for i in 0..k {
                        let r = &b[at + 15 * i..at + 15 * (i + 1)];
                        side.push(SideLog { turns: [get_turn(&r[0..6]), get_turn(&r[6..12])], q: r[12] as i8 as f32 / 127.0, lines: u16::from_le_bytes([r[13], r[14]]) as u32 });
                    }
                    at += 15 * k;
                }
                ticks.push(TickLog { turns, q: if flags & 2 != 0 { Some(q) } else { None }, pol, side });
            }
            games.push(GameLog { seed, z, ticks });
        }
        games
    }

    /// Replays the game and adds its searched positions to `out`, and after each the positions
    /// beside the path that its tick carries (value only: the search's value of them is all
    /// there is to learn, the game never went there). False when the replay does not end as
    /// the game did (a file from another engine or map): nothing is added then.
    pub fn samples(&self, e: &Engine, out: &mut Vec<Sample>) -> bool {
        let mut s = e.new_match(self.seed);
        let start = out.len();
        for t in &self.ticks {
            if s.result.is_some() {
                break;
            }
            if let Some(pol) = &t.pol {
                out.push(Sample { s, pol: *pol, q: t.q.unwrap_or(0.0), has_q: t.q.is_some(), z: self.z, side: false });
            }
            for k in &t.side {
                let mut beside = s;
                e.step(&mut beside, &k.turns);
                if beside.result.is_none() {
                    out.push(Sample { s: beside, pol: [[0; CLASSES]; 2], q: k.q, has_q: true, z: 0, side: true });
                }
            }
            e.step(&mut s, &t.turns);
        }
        let z = crate::pvsearch::outcome(&s).map(|v| v as i8);
        if z != Some(self.z) {
            out.truncate(start);
            return false;
        }
        true
    }
}
