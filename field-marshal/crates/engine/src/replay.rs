//! Replay files and their verification, ported from `packages/engine/src/replay.ts`.
//!
//! A replay is JSON Lines: a header (rules, map, seed), one line per tick with both
//! fighters' turns and the hash of the state after the tick, and a result line.
//! Re-simulating a replay and matching every hash is the proof that this port is exact.

use crate::hash::canonical_json;
use crate::turn::RawTurn;
use crate::{Engine, State, ENGINE_VERSION};
use serde_json::Value;

pub struct ReplayTick {
    pub tick: i64,
    pub turns: [RawTurn; 2],
    pub hash: Option<String>,
    /// optional full state after the tick (written by the fuzz oracle), as canonical JSON
    pub state: Option<String>,
    /// optional official observations after the tick, by fighter index (fuzz oracle, --observations)
    pub obs: [Option<Value>; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedResult {
    pub winner: Option<i64>,
    pub reason: String,
    pub final_tick: i64,
}

pub struct Replay {
    pub engine: Engine,
    pub seed: i64,
    pub bots: Vec<String>,
    pub ticks: Vec<ReplayTick>,
    pub result: Option<RecordedResult>,
}

pub fn parse_replay(text: &str) -> Result<Replay, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: Value =
        serde_json::from_str(lines.next().ok_or("replay: missing header")?).map_err(|e| format!("replay: header: {e}"))?;
    if header.get("type").and_then(Value::as_str) != Some("header") {
        return Err("replay: missing header".into());
    }
    if let Some(f) = header.get("replay_format") {
        if f.as_i64() != Some(1) {
            return Err(format!("replay: unsupported format {f}"));
        }
    }
    let engine_version = header.get("engine").and_then(Value::as_str).unwrap_or("?");
    if engine_version != ENGINE_VERSION {
        return Err(format!("replay: recorded with engine {engine_version}, this port is {ENGINE_VERSION}"));
    }
    let rules_json = header.get("rules").ok_or("replay: rules missing")?;
    let map_json = header.get("map_data").ok_or("replay: map_data missing")?;
    let engine = Engine::from_json(rules_json, map_json)?;
    if header.get("rules_hash").and_then(Value::as_str) != Some(engine.rules.hash.as_str()) {
        return Err("replay: rules hash mismatch".into());
    }
    let seed = header.get("seed").and_then(Value::as_i64).ok_or("replay: seed missing")?;
    let bots = header
        .get("bots")
        .and_then(Value::as_array)
        .map(|b| b.iter().map(|x| x.get("name").and_then(Value::as_str).unwrap_or("?").to_string()).collect())
        .unwrap_or_default();

    let mut ticks = Vec::new();
    let mut result = None;
    for line in lines {
        let obj: Value = serde_json::from_str(line).map_err(|e| format!("replay: {e}"))?;
        match obj.get("type").and_then(Value::as_str) {
            Some("tick") => {
                let turns = obj.get("turns");
                let turn = |id: &str| RawTurn::from_json(turns.and_then(|t| t.get(id)), &engine.rules);
                ticks.push(ReplayTick {
                    tick: obj.get("tick").and_then(Value::as_i64).ok_or("replay: tick number missing")?,
                    turns: [turn("1"), turn("2")],
                    hash: obj.get("hash").and_then(Value::as_str).map(str::to_string),
                    state: obj.get("state").map(|s| {
                        let mut out = String::new();
                        canonical_json(s, &mut out);
                        out
                    }),
                    obs: [
                        obj.get("obs").and_then(|o| o.get("1")).cloned(),
                        obj.get("obs").and_then(|o| o.get("2")).cloned(),
                    ],
                });
            }
            Some("result") => {
                result = Some(RecordedResult {
                    winner: obj.get("winner").and_then(Value::as_i64),
                    reason: obj.get("reason").and_then(Value::as_str).unwrap_or("?").to_string(),
                    final_tick: obj.get("final_tick").and_then(Value::as_i64).unwrap_or(-1),
                });
            }
            _ => {}
        }
    }
    Ok(Replay { engine, seed, bots, ticks, result })
}

pub struct Verification {
    /// ticks re-simulated
    pub ticks: usize,
    /// ticks whose hash was compared
    pub hashes_checked: usize,
    /// official observations compared with the port's
    pub observations_checked: usize,
    pub final_state: State,
}

/// Why a replay failed to verify.
pub struct Divergence {
    pub tick: i64,
    pub message: String,
    /// our canonical state JSON at the diverging tick, and the oracle's when the replay carries it
    pub ours: Option<String>,
    pub theirs: Option<String>,
}

/// Re-simulates a replay, checking every recorded hash and the recorded result.
pub fn verify_replay(r: &Replay) -> Result<Verification, Divergence> {
    let e = &r.engine;
    let mut s = e.new_match(r.seed);
    let mut json = String::with_capacity(4096);
    let mut hashes_checked = 0;
    let mut observations_checked = 0;
    let mut events = Vec::new();
    let mut ticks = 0;
    for t in &r.ticks {
        if s.result.is_some() {
            break;
        }
        if t.tick != s.tick as i64 {
            return Err(Divergence {
                tick: t.tick,
                message: format!("expected tick {}, found {}", s.tick, t.tick),
                ours: None,
                theirs: None,
            });
        }
        e.step_events(&mut s, &t.turns, &mut events);
        ticks += 1;
        e.state_json(&s, &mut json);
        if let Some(h) = &t.hash {
            hashes_checked += 1;
            if crate::hash::hash_string(&json) != *h {
                return Err(Divergence {
                    tick: t.tick,
                    message: "state hash diverged".into(),
                    ours: Some(json.clone()),
                    theirs: t.state.clone(),
                });
            }
        }
        for (i, obs) in t.obs.iter().enumerate() {
            if let Some(obs) = obs {
                observations_checked += 1;
                let theirs = crate::observe::View::from_json(e, obs).map_err(|m| Divergence {
                    tick: t.tick,
                    message: format!("fighter {}: {m}", i + 1),
                    ours: None,
                    theirs: None,
                })?;
                // The events this fighter is told about, in order. A shout's text is not kept by the port.
                let mut told = obs.get("events").cloned().unwrap_or(Value::Array(vec![]));
                if let Some(list) = told.as_array_mut() {
                    for ev in list {
                        if ev.get("kind").and_then(Value::as_str) == Some("shout") {
                            ev["text"] = Value::Null;
                        }
                    }
                }
                let mine = Value::Array(crate::events::visible_events(e, &s, i, &events).iter().map(|x| x.to_json(e)).collect());
                let (mut a, mut b) = (String::new(), String::new());
                canonical_json(&mine, &mut a);
                canonical_json(&told, &mut b);
                if a != b {
                    return Err(Divergence {
                        tick: t.tick,
                        message: format!("events for fighter {} diverged", i + 1),
                        ours: Some(a),
                        theirs: Some(b),
                    });
                }
                let ours = crate::observe::View::from_state(e, &s, i);
                if ours != theirs {
                    return Err(Divergence {
                        tick: t.tick,
                        message: format!("observation for fighter {} diverged", i + 1),
                        ours: Some(format!("{ours:?}")),
                        theirs: Some(format!("{theirs:?}")),
                    });
                }
            }
        }
        if let Some(theirs) = &t.state {
            if *theirs != json {
                return Err(Divergence {
                    tick: t.tick,
                    message: "state diverged".into(),
                    ours: Some(json.clone()),
                    theirs: Some(theirs.clone()),
                });
            }
        }
    }
    let ours = s.result.map(|x| RecordedResult {
        winner: x.winner.map(|w| w as i64),
        reason: x.reason.name().to_string(),
        final_tick: x.final_tick as i64,
    });
    let fail = |message: String| Divergence { tick: s.tick as i64, message, ours: None, theirs: None };
    match &r.result {
        // Forfeits and disqualifications are decided by the server, not the simulation:
        // they interrupt the next unresolved tick of a running match.
        Some(rec) if rec.reason == "forfeit" || rec.reason == "disqualified" => {
            let valid_winner = match rec.winner {
                None => rec.reason == "forfeit",
                Some(w) => (1..=2).contains(&w) && s.fighters[(w - 1) as usize].alive,
            };
            if ours.is_some() || rec.final_tick != s.tick as i64 || !valid_winner {
                return Err(fail(format!("recorded {rec:?} does not fit the simulated match (ours {ours:?}, tick {})", s.tick)));
            }
        }
        Some(rec) => {
            if ours.as_ref() != Some(rec) {
                return Err(fail(format!("result differs: recorded {rec:?}, ours {ours:?}")));
            }
        }
        None => {
            // A replay without a result line (the fuzz oracle may stop early) is checked on hashes alone.
        }
    }
    Ok(Verification { ticks, hashes_checked, observations_checked, final_state: s })
}
