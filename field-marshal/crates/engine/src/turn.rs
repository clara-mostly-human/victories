//! A fighter's requested turn, before validation.
//!
//! `RawTurn` keeps just enough of what a bot sent to reproduce the engine's validation
//! (`packages/engine/src/validate.ts`) exactly, including turns that are malformed. A
//! search builds turns with the constructors below and never needs the malformed cases.

use crate::geom::{dir_from_name, P};
use crate::rules::Rules;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DirIn {
    /// absent or null
    #[default]
    None,
    /// present but not one of the eight directions
    Bad,
    Dir(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetIn {
    /// absent or null
    Missing,
    /// not `[x, y]` integers within ±1000
    Bad,
    Tile(P),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AttackIn {
    #[default]
    None,
    /// present, but its name is not in the rules' attack table
    Unknown,
    Named { attack: u8, target: TargetIn },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ShoutIn {
    #[default]
    None,
    /// not a string, or blank
    Bad,
    /// a non-blank string of this many code points
    Text { chars: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RawTurn {
    pub mv: DirIn,
    pub dash: DirIn,
    pub attack: AttackIn,
    pub shout: ShoutIn,
}

impl RawTurn {
    pub const IDLE: RawTurn =
        RawTurn { mv: DirIn::None, dash: DirIn::None, attack: AttackIn::None, shout: ShoutIn::None };

    pub fn step(dir: u8) -> RawTurn {
        RawTurn { mv: DirIn::Dir(dir), ..RawTurn::IDLE }
    }
    pub fn dash(dir: u8) -> RawTurn {
        RawTurn { dash: DirIn::Dir(dir), ..RawTurn::IDLE }
    }
    /// Adds an aimed attack (ranged or frostburst).
    pub fn with_attack(mut self, attack: u8, target: P) -> RawTurn {
        self.attack = AttackIn::Named { attack, target: TargetIn::Tile(target) };
        self
    }
    /// Adds an attack with no aim (the repulsor).
    pub fn with_burst(mut self, attack: u8) -> RawTurn {
        self.attack = AttackIn::Named { attack, target: TargetIn::Missing };
        self
    }

    /// Reads a turn as the engine would read the same JSON.
    pub fn from_json(v: Option<&Value>, rules: &Rules) -> RawTurn {
        let o = match v.and_then(Value::as_object) {
            Some(o) => o,
            None => return RawTurn::IDLE, // missing, null, or not an object: an idle turn
        };
        let present = |k: &str| o.get(k).filter(|x| !x.is_null());
        let dir = |k: &str| match present(k) {
            None => DirIn::None,
            Some(x) => match x.as_str().and_then(dir_from_name) {
                Some(d) => DirIn::Dir(d),
                None => DirIn::Bad,
            },
        };
        let attack = match present("attack") {
            None => AttackIn::None,
            Some(a) => match a.get("name").and_then(Value::as_str).and_then(|n| rules.attack_index(n)) {
                None => AttackIn::Unknown,
                Some(i) => AttackIn::Named {
                    attack: i,
                    target: match a.get("target").filter(|x| !x.is_null()) {
                        None => TargetIn::Missing,
                        Some(t) => match as_tile(t) {
                            Some(p) => TargetIn::Tile(p),
                            None => TargetIn::Bad,
                        },
                    },
                },
            },
        };
        let shout = match present("shout") {
            None => ShoutIn::None,
            Some(s) => match s.as_str() {
                Some(text) if !js_trim_is_empty(text) => ShoutIn::Text { chars: text.chars().count() as u32 },
                _ => ShoutIn::Bad,
            },
        };
        RawTurn { mv: dir("move"), dash: dir("dash"), attack, shout }
    }

    /// The turn in four bytes, for a search tree that holds millions of them (as it stands the
    /// type takes 28). None for a turn that does not fit: one with a shout, a direction or an
    /// attack past the eighth, or a target more than 64 tiles before the board's first tile or
    /// 191 past it. Every turn a search builds with the constructors above fits.
    pub fn pack(&self) -> Option<u32> {
        let dir = |d: DirIn| match d {
            DirIn::None => Some(0u32),
            DirIn::Bad => Some(1),
            DirIn::Dir(d) if d < 8 => Some(2 + d as u32),
            DirIn::Dir(_) => None,
        };
        if self.shout != ShoutIn::None {
            return None;
        }
        let attack = match self.attack {
            AttackIn::None => 0u32,
            AttackIn::Unknown => 1,
            AttackIn::Named { attack, target } => {
                if attack >= 8 {
                    return None;
                }
                let id = (attack as u32) << 3;
                match target {
                    TargetIn::Missing => 2 | id,
                    TargetIn::Bad => 3 | id,
                    TargetIn::Tile([x, y]) => {
                        if !(-64..192).contains(&x) || !(-64..192).contains(&y) {
                            return None;
                        }
                        4 | id | ((x + 64) as u32) << 6 | ((y + 64) as u32) << 14
                    }
                }
            }
        };
        Some(dir(self.mv)? | dir(self.dash)? << 4 | attack << 8)
    }

    /// The turn `pack` made `code` from.
    pub fn unpack(code: u32) -> RawTurn {
        let dir = |c: u32| match c & 15 {
            0 => DirIn::None,
            1 => DirIn::Bad,
            d => DirIn::Dir((d - 2) as u8),
        };
        let a = code >> 8;
        let id = (a >> 3 & 7) as u8;
        let attack = match a & 7 {
            0 => AttackIn::None,
            1 => AttackIn::Unknown,
            2 => AttackIn::Named { attack: id, target: TargetIn::Missing },
            3 => AttackIn::Named { attack: id, target: TargetIn::Bad },
            _ => AttackIn::Named { attack: id, target: TargetIn::Tile([(a >> 6 & 255) as i32 - 64, (a >> 14 & 255) as i32 - 64]) },
        };
        RawTurn { mv: dir(code), dash: dir(code >> 4), attack, shout: ShoutIn::None }
    }
}

/// `[x, y]` with both integers within ±1000.
fn as_tile(v: &Value) -> Option<P> {
    let a = v.as_array()?;
    if a.len() != 2 {
        return None;
    }
    let coord = |x: &Value| -> Option<i32> {
        let f = x.as_f64()?; // strings, booleans and null are not numbers
        if f.fract() != 0.0 || f.abs() > 1000.0 {
            return None;
        }
        Some(f as i32)
    };
    Some([coord(&a[0])?, coord(&a[1])?])
}

/// Would JavaScript's `text.trim()` be empty?
fn js_trim_is_empty(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(
            c,
            '\u{9}'..='\u{d}'
                | ' '
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packed_turn_comes_back_as_it_went_in() {
        let dirs = [DirIn::None, DirIn::Bad, DirIn::Dir(0), DirIn::Dir(3), DirIn::Dir(7)];
        let mut attacks = vec![AttackIn::None, AttackIn::Unknown];
        for attack in [0u8, 1, 2, 7] {
            attacks.push(AttackIn::Named { attack, target: TargetIn::Missing });
            attacks.push(AttackIn::Named { attack, target: TargetIn::Bad });
            for x in [-64, -3, 0, 10, 20, 40, 191] {
                for y in [-64, -1, 0, 7, 20, 43, 191] {
                    attacks.push(AttackIn::Named { attack, target: TargetIn::Tile([x, y]) });
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for mv in dirs {
            for dash in dirs {
                for attack in &attacks {
                    let t = RawTurn { mv, dash, attack: *attack, shout: ShoutIn::None };
                    let code = t.pack().expect("fits");
                    assert_eq!(RawTurn::unpack(code), t);
                    assert!(seen.insert(code), "two turns with one code");
                }
            }
        }
        // what does not fit is refused, not changed
        assert_eq!(RawTurn { shout: ShoutIn::Bad, ..RawTurn::IDLE }.pack(), None);
        assert_eq!(RawTurn { shout: ShoutIn::Text { chars: 3 }, ..RawTurn::IDLE }.pack(), None);
        assert_eq!(RawTurn::step(8).pack(), None);
        assert_eq!(RawTurn::dash(200).pack(), None);
        assert_eq!(RawTurn::IDLE.with_burst(8).pack(), None);
        assert_eq!(RawTurn::IDLE.with_attack(0, [192, 0]).pack(), None);
        assert_eq!(RawTurn::IDLE.with_attack(0, [0, -65]).pack(), None);
    }
}
