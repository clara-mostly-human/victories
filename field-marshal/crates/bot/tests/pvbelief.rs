//! `tree_pv_fog_belief`: the learned search deciding hidden ticks over its guesses for as long
//! as the guesses hold enough of the bot's belief about where the opponent stands, with no
//! count of ticks. Whole games on the exact engine with sight blocked by the walls, as on the
//! ladder; the arena knows the true state, so the belief can be scored.
//!
//! Played with the columns-21 network kept in `models/pv`. The test does not ask how well the
//! bot does with the switch (the arena measures that over many games). It checks that the
//! belief is moved on through hidden ticks well past the ten of `tree_pv_fog_ticks`, that the
//! search decides only where the guesses hold the share asked for and leaves the other ticks
//! alone, that no guess is ever set down where we would be seeing the opponent, and that the
//! share the belief gives the guesses is an honest one: the opponent really is on one of
//! their tiles about that often.
//!
//! In a file of its own: the counters are one set for the whole process.

use marshal::arena::play;
use marshal::policy::{Playbook, PV_GUESS_STATS};
use serde_json::json;
use std::sync::atomic::Ordering::Relaxed;
use victris_engine::Engine;

#[test]
fn the_learned_search_decides_hidden_ticks_while_its_guesses_hold_the_belief() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/pv");
    let book = |belief: f64, ticks: u32| Playbook::from_json("test", &json!({ "tree_ms": 20, "tree_iters": 600, "tree_pv_dir": dir.to_str().unwrap(), "tree_pv_fog_belief": belief, "tree_pv_fog_ticks": ticks }));
    // the share asked for decides, whatever count of ticks is set beside it
    let (with, without) = (book(0.5, 10), book(0.0, 0));
    assert_eq!(with.tree_pv_fog_belief, 0.5);
    assert_eq!(without.tree_pv_fog_belief, 0.0);
    let e = Engine::shipped("columns-21");
    let stats = || PV_GUESS_STATS.iter().map(|a| a.load(Relaxed) as f64).collect::<Vec<f64>>();

    // without the switch there is no belief and nothing is decided over guesses
    let g = play(&e, 11, [&without, &without]);
    assert_eq!(g.mismatches, 0);
    assert!(stats().iter().all(|x| *x == 0.0), "a belief was kept with the switch off");

    // both seats with the switch: each hides from the other on the same ticks
    let mut mismatches = 0;
    for seed in [11, 12, 13] {
        let g = play(&e, seed, [&with, &with]);
        println!("seed {seed}: {} ticks, winner {:?}", g.ticks, g.winner);
        mismatches += g.mismatches;
    }
    assert_eq!(mismatches, 0, "the tracker lost the game with the switch on");
    let s = stats();
    let (moved, searched, left) = (s[8], s[0], s[11]);
    let (held, on_one) = (s[9] / 1e6 / moved.max(1.0), s[10] / moved.max(1.0));
    println!(
        "the belief moved on {moved} hidden ticks with {:.1} places; the guesses held {:.0}% of it and the opponent was on one of their tiles {:.0}%; {searched} ticks decided over guesses ({:.1} a tick), {left} left alone",
        s[12] / moved.max(1.0),
        100.0 * held,
        100.0 * on_one,
        s[6] / searched.max(1.0)
    );
    assert!(moved >= 300.0, "the belief was moved on only {moved} hidden ticks in 3 games");
    assert!(searched >= 100.0, "only {searched} hidden ticks were decided over guesses in 3 games");
    assert!(left >= 20.0, "only {left} hidden ticks were left to the play without the switch: the share asked for stops nothing");
    assert!(searched + left <= moved);
    assert_eq!(s[7], 0.0, "a guess put the opponent on a tile we could see");
    assert!(s[12] / moved > 3.0, "the belief holds no more than the guesses searched ({:.1} places)", s[12] / moved);
    // an honest belief: the opponent is on one of the guesses' tiles about as often as it says
    assert!((on_one - held).abs() <= 0.15, "the belief gives the guesses {:.0}% and the opponent is on one of their tiles {:.0}% of the time", 100.0 * held, 100.0 * on_one);
    assert!(s[3] > s[5], "the likeliest guess is within one tile of the opponent on {} ticks, the standing ghost on {}", s[3], s[5]);
    assert!(s[6] <= 3.0 * searched, "more than three guesses a tick were searched");
    // the whole belief, not only the guesses, gives the opponent's real tile a fair share
    assert!(s[14] / 1e6 / moved >= 0.2, "{:.0}% of the belief was on the opponent's real tile", s[14] / 1e4 / moved);
}
