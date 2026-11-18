//! Canonical JSON and cyrb53, ported from `packages/engine/src/util.ts`.
//!
//! The ladder hashes every tick's state as cyrb53 over canonical JSON (keys sorted, no
//! whitespace). Reproducing those hashes is how this port is proven exact.

use serde_json::Value;

/// cyrb53 over UTF-16 code units, formatted as 14 hex digits.
pub fn hash_units(units: impl Iterator<Item = u16>) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for ch in units {
        h1 = (h1 ^ ch as u32).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ ch as u32).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    let v = (((h2 & 0x1f_ffff) as u64) << 32) | h1 as u64;
    format!("{v:014x}")
}

pub fn hash_string(s: &str) -> String {
    if s.is_ascii() {
        hash_units(s.bytes().map(|b| b as u16))
    } else {
        hash_units(s.encode_utf16())
    }
}

/// Canonical JSON of an arbitrary value: object keys sorted, no whitespace.
pub fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                let f = n.as_f64().unwrap_or(f64::NAN);
                if !f.is_finite() {
                    out.push_str("null");
                } else if f.fract() == 0.0 && f.abs() < 1e15 {
                    out.push_str(&(f as i64).to_string());
                } else {
                    out.push_str(&f.to_string());
                }
            }
        }
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap()),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(x, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            // Sort by UTF-16 code units, as JavaScript's default sort does.
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                canonical_json(&o[*k], out);
            }
            out.push('}');
        }
    }
}

pub fn hash_value(v: &Value) -> String {
    let mut s = String::new();
    canonical_json(v, &mut s);
    hash_string(&s)
}
