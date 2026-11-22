//! `tree_pv_fog_ticks`: the learned search deciding while the opponent is out of sight, over its
//! own guesses of where the opponent stands. Whole games on the exact engine with sight blocked
//! by the walls, as on the ladder; the arena knows the true state, so the guesses can be scored.
//!
//! Played with the columns-21 network kept in `models/pv`. The test does not ask how well the
//! bot does with the switch (the arena measures that over many games); it checks that the
//! guesses are kept and moved on every hidden tick the switch covers, that none is ever set
//! down where we would be seeing the opponent, and that they follow the opponent better than
//! the tracker's ghost, which stands still where the opponent was last seen.

use marshal::arena::play;
use marshal::policy::{Playbook, PV_GUESS_STATS};
use serde_json::json;
use std::sync::atomic::Ordering::Relaxed;
use victris_engine::Engine;

#[test]
fn the_learned_search_keeps_deciding_over_guesses_when_the_opponent_hides() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/pv");
    let book = |ticks: u32| Playbook::from_json("test", &json!({ "tree_ms": 20, "tree_iters": 600, "tree_pv_dir": dir.to_str().unwrap(), "tree_pv_fog_ticks": ticks }));
    let (with, without) = (book(10), book(0));
    let e = Engine::shipped("columns-21");

    // without the switch nothing is decided over guesses
    let g = play(&e, 11, [&without, &without]);
    assert_eq!(g.mismatches, 0);
    assert_eq!(PV_GUESS_STATS[0].load(Relaxed), 0, "guesses were used with the switch off");

    let mut mismatches = 0;
    for seed in [11, 12] {
        for books in [[&with, &without], [&without, &with]] {
            let g = play(&e, seed, books);
            println!("seed {seed}: {} ticks, winner {:?}", g.ticks, g.winner);
            mismatches += g.mismatches;
        }
    }
    assert_eq!(mismatches, 0, "the tracker lost the game with the switch on");
    let s: Vec<f64> = PV_GUESS_STATS.iter().map(|a| a.load(Relaxed) as f64).collect();
    let (ticks, weight_on_it, near, never_in_sight) = (s[0], s[1] / 1e6 / s[0].max(1.0), s[3] / s[0].max(1.0), s[7] == 0.0);
    println!("{ticks} hidden ticks decided over guesses, {:.1} guesses a tick; weight on the true tile {:.0}%, likeliest guess within one tile {:.0}% (the standing ghost {:.0}%)", s[6] / ticks.max(1.0), 100.0 * weight_on_it, 100.0 * near, 100.0 * s[5] / ticks.max(1.0));
    assert!(ticks >= 100.0, "only {ticks} hidden ticks were decided over guesses in 4 games");
    assert!(never_in_sight, "a guess put the opponent on a tile we could see");
    assert!(weight_on_it >= 0.2, "the guesses give the opponent's true tile {:.0}% of their weight", 100.0 * weight_on_it);
    assert!(near >= 0.6, "the likeliest guess is within one tile of the opponent only {:.0}% of the time", 100.0 * near);
    assert!(s[3] > s[5], "the likeliest guess is within one tile of the opponent on {} ticks, the standing ghost on {}", s[3], s[5]);
}
