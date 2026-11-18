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
