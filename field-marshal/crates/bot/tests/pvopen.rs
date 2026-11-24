//! `tree_pv_open`: the learned search deciding the ticks before the opponent is first seen, over
//! its own guesses, which start from the opponent's spawn. Whole games on the exact engine with
//! sight blocked by the walls, as on the ladder; the arena knows the true state, so the guesses
//! can be scored.
//!
//! Played with the columns-21 network kept in `models/pv`. The test does not ask how well the
//! bot does with the switch (that depends on the network and is measured over many games); it
//! checks that with it off the opening is played as before, that with it on (and
//! `tree_pv_fog_ticks` off) guesses are used on the opening ticks and only there, and that none
//! is ever set down where we would be seeing the opponent.

use marshal::arena::play;
use marshal::policy::{Playbook, PV_GUESS_STATS};
use serde_json::json;
use std::sync::atomic::Ordering::Relaxed;
use victris_engine::Engine;

#[test]
fn the_learned_search_plays_the_ticks_before_first_sight() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/pv");
    let book = |open: bool| Playbook::from_json("test", &json!({ "tree_ms": 20, "tree_iters": 600, "tree_pv_dir": dir.to_str().unwrap(), "tree_pv_open": open }));
    let (with, without) = (book(true), book(false));
    assert!(!Playbook::from_json("test", &json!({})).tree_pv_open, "the switch must be off unless asked for");
    let e = Engine::shipped("columns-21");

    // without the switch nothing is decided over guesses
    let g = play(&e, 11, [&without, &without]);
    assert_eq!(g.mismatches, 0);
    assert_eq!(PV_GUESS_STATS[0].load(Relaxed), 0, "guesses were used with the switch off");

    let mut mismatches = 0;
    let games = 4.0;
    for seed in [11, 12] {
        for books in [[&with, &without], [&without, &with]] {
            let g = play(&e, seed, books);
            println!("seed {seed}: {} ticks, winner {:?}", g.ticks, g.winner);
            mismatches += g.mismatches;
        }
    }
    assert_eq!(mismatches, 0, "the tracker lost the game with the switch on");
    let s: Vec<f64> = PV_GUESS_STATS.iter().map(|a| a.load(Relaxed) as f64).collect();
    let ticks = s[0];
    println!(
        "{ticks} opening ticks decided over guesses in {games} games, {:.1} guesses a tick; weight on the true tile {:.0}%, likeliest guess within one tile {:.0}% (the standing ghost {:.0}%)",
        s[6] / ticks.max(1.0),
        100.0 * s[1] / 1e6 / ticks.max(1.0),
        100.0 * s[3] / ticks.max(1.0),
        100.0 * s[5] / ticks.max(1.0)
    );
    // the spawns of columns-21 are 14 tiles apart behind columns: first sight takes some ticks,
    // and nothing after it is decided over guesses when `tree_pv_fog_ticks` is off
    assert!(ticks >= 4.0 * games, "only {ticks} opening ticks were decided over guesses in {games} games");
    assert!(ticks <= 80.0 * games, "{ticks} ticks were decided over guesses in {games} games: more than the openings");
    assert_eq!(s[7], 0.0, "a guess put the opponent on a tile we could see");
}
