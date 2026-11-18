//! Tracker regression test on a recorded run: eight local games played by `marshal` over a
//! WebSocket against the official simulator's match core (tools/gauntlet.ts --record),
//! with the true match state attached to every observation.
//!
//! The recorded turns are replayed into the tracker (not re-decided), so this test does not
//! depend on the policy. It checks the two things the tracker promises: no observation goes
//! unexplained while the opponent is in view, and what the tracker believes about itself
//! always equals the truth.

use flate2::read::GzDecoder;
use marshal::tracker::{Sync, Tracker};
use serde_json::Value;
use std::io::Read;
use victris_engine::observe::View;
use victris_engine::turn::RawTurn;

#[test]
fn recorded_games_are_tracked_without_mismatch() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/gauntlet-8-games.jsonl.gz");
    let mut text = String::new();
    GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut text).unwrap();

    let mut tracker: Option<Tracker> = None;
    let mut last: Option<(i32, RawTurn)> = None;
    let (mut games, mut ticks, mut mismatches, mut seen, mut own_exact, mut whole_exact) = (0, 0, 0, 0, 0, 0);
    for line in text.lines() {
        let rec: Value = serde_json::from_str(line).unwrap();
        let frame = &rec["frame"];
        let from_server = rec["dir"] == "server";
        match (from_server, frame.get("type").and_then(Value::as_str)) {
            (true, Some("hello")) => {
                tracker = Some(Tracker::new(frame).expect("hello loads"));
                last = None;
                games += 1;
            }
            (true, Some("observation")) => {
                let t = tracker.as_mut().unwrap();
                let view = View::from_json(&t.engine, frame).expect("observation parses");
                let events = frame["events"].as_array().cloned().unwrap_or_default();
                let prev = last.filter(|(tick, _)| tick + 1 == view.tick).map(|(_, turn)| turn);
                let sync = t.observe(&view, &events, prev);
                ticks += 1;
                if sync == Sync::Mismatch {
                    mismatches += 1;
                    eprintln!("mismatch at tick {}: {:?}", view.tick, t.last_mismatch);
                }
                // Compare with the truth the harness attached.
                let truth = &frame["_truth"];
                let belief = t.belief.as_ref().unwrap();
                let mut ours = String::new();
                t.engine.state_json(belief, &mut ours);
                let ours: Value = serde_json::from_str(&ours).unwrap();
                let me = t.me;
                let strip = |f: &Value| {
                    let mut f = f.clone();
                    f["state_end"] = Value::Null; // a leftover the rules do not read while idle
                    f
                };
                if strip(&ours["fighters"][me]) == strip(&truth["fighters"][me]) {
                    own_exact += 1;
                }
                if view.enemy.is_some() {
                    seen += 1;
                    let mut a = ours.clone();
                    let mut b = truth.clone();
                    for v in [&mut a, &mut b] {
                        // Hidden even in view: the opponent's step and weapon cooldowns, pads
                        // out of sight, and shots still out of sight.
                        v["fighters"][1 - me]["cooldowns"] = Value::Null;
                        v["fighters"][1 - me]["state_end"] = Value::Null;
                        v["fighters"][me]["state_end"] = Value::Null;
                        v["pickups"] = Value::Null;
                        v["projectiles"] = Value::Null;
                        v["areas"] = Value::Null;
                        v["next_projectile_id"] = Value::Null;
                    }
                    if a == b {
                        whole_exact += 1;
                    } else {
                        eprintln!("tick {} ({sync:?}):\n  ours  {a}\n  truth {b}", view.tick);
                    }
                }
            }
            (false, None) | (false, Some("turn")) => {
                // A turn the bot sent: what the server played for us on that tick.
                if let (Some(t), Some(tick)) = (tracker.as_ref(), frame.get("tick").and_then(Value::as_i64)) {
                    last = Some((tick as i32, RawTurn::from_json(Some(frame), &t.engine.rules)));
                }
            }
            _ => {}
        }
    }
    eprintln!("{games} games, {ticks} ticks, {mismatches} mismatches; own fighter exact {own_exact}; opponent in view {seen}, visible state exact {whole_exact}");
    assert_eq!(games, 8);
    assert!(ticks > 1500);
    assert_eq!(mismatches, 0, "an observation went unexplained with the opponent in view");
    assert_eq!(own_exact, ticks, "the tracker's own fighter must always equal the truth");
    assert_eq!(whole_exact, seen, "with the opponent in view, everything visible must equal the truth");
}
