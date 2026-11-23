//! `tree_pv_plan`: with the opponent out of sight and past the learned search's guesses, walks
//! scored by the network's value decide instead of the hand-written policy. Whole games on the
//! exact engine with sight blocked by the walls, as on the ladder.
//!
//! Played with the pillars-21 network kept in `models/pv`. The test does not ask how well the
//! bot does with the switch (that depends on the network and is measured over many games); it
//! checks that with it off no walk is tried, that with it on walks decide hidden ticks and take
//! steps, that the trackers keep the game, and that the same game comes out twice.

use marshal::arena::play;
use marshal::policy::{Playbook, PV_PLAN_STATS};
use serde_json::json;
use std::sync::atomic::Ordering::Relaxed;
use victris_engine::Engine;

#[test]
fn walks_by_the_network_play_the_long_hidden_ticks() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/pv");
    let book = |plan: u32| Playbook::from_json("test", &json!({ "tree_ms": 600, "tree_iters": 600, "tree_pv_dir": dir.to_str().unwrap(), "tree_pv_fog_ticks": 10, "tree_pv_plan": plan }));
    let (with, without) = (book(24), book(0));
    assert_eq!(Playbook::from_json("test", &json!({})).tree_pv_plan, 0, "the switch must be off unless asked for");
    let e = Engine::shipped("pillars-21");

    let g = play(&e, 11, [&without, &without]);
    assert_eq!(g.mismatches, 0);
    assert_eq!(PV_PLAN_STATS[0].load(Relaxed), 0, "walks were tried with the switch off");

    let first = play(&e, 11, [&with, &without]);
    let s: Vec<u64> = PV_PLAN_STATS.iter().map(|a| a.load(Relaxed)).collect();
    println!("{} ticks, winner {:?}: {} decisions by walks, {} of them a step, {:.0} walks tried a decision", first.ticks, first.winner, s[0], s[1], s[2] as f64 / s[0].max(1) as f64);
    assert_eq!(first.mismatches, 0, "the tracker lost the game with the switch on");
    // the spawns of pillars-21 are out of each other's sight: the opening alone is some ticks
    assert!(s[0] >= 5, "only {} decisions were taken by walks in a game", s[0]);
    assert!(s[1] >= 3, "the walks took only {} steps in a game", s[1]);
    assert!(s[2] >= 20 * s[0], "{} walks for {} decisions: too few tiles within reach", s[2], s[0]);

    // nothing in a walk is random: the same seats and seed give the same game
    let again = play(&e, 11, [&with, &without]);
    assert_eq!((first.ticks, first.winner, first.stats), (again.ticks, again.winner, again.stats), "the same game came out differently");
    let other = play(&e, 12, [&without, &with]);
    assert_eq!(other.mismatches, 0, "the tracker lost the game with the switch on (seat 1)");
}
