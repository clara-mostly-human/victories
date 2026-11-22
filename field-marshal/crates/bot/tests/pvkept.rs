//! What the learned search keeps between ticks (its tree, its guesses at a hidden opponent)
//! belongs to the match being played: a finished match's is dropped when the next one starts,
//! and `pv_forget` drops it at once. (In a file of its own: the guesses' counters are shared
//! by every test of a file.)

use marshal::arena::play;
use marshal::policy::{pv_forget, pv_kept, Playbook};
use serde_json::json;
use victris_engine::Engine;

#[test]
fn only_the_match_being_played_keeps_a_tree() {
    // the learned search keeps its tree (and its guesses at a hidden opponent) from one tick to
    // the next. A finished match's last tree used to stay until sixteen had piled up.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/pv");
    let book = Playbook::from_json("test", &json!({ "tree_ms": 20, "tree_iters": 300, "tree_pv_dir": dir.to_str().unwrap(), "tree_pv_fog_ticks": 10, "tree_pv_nodes": 2000 }));
    let e = Engine::shipped("columns-21");
    for seed in [21, 22, 23] {
        let g = play(&e, seed, [&book, &book]);
        assert_eq!(g.mismatches, 0);
        let (trees, guesses) = pv_kept();
        assert!(trees <= 2 && guesses <= 2, "after game {seed}: {trees} trees and {guesses} sets of guesses kept (two seats play here)");
    }
    assert!(pv_kept().0 >= 1, "no tree was kept at all");
    pv_forget();
    assert_eq!(pv_kept(), (0, 0));
}
