//! Pondering (`Search::ponder`): every extra iteration goes under the turn we already sent.

use victris_engine::mcts::{Config, Search};
use victris_engine::Engine;

#[test]
fn ponder_spends_every_iteration_under_our_turn_and_grows_what_carries_over() {
    let e = Engine::shipped("columns-21");
    let mut s = e.new_match(7);
    let mut rng = 0x9e3779b97f4a7c15u64 | 1;
    // a position some ticks in, both fighters alive
    for _ in 0..30 {
        let turns = [victris_engine::clone::clone_turn(&e, &s, 0, &mut rng), victris_engine::clone::clone_turn(&e, &s, 1, &mut rng)];
        e.step(&mut s, &turns);
    }
    assert!(s.result.is_none());
    let cfg = Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, slam_prior: 0.228, ..Default::default() };
    for me in 0..2 {
        let mut search = Search::new(&e, cfg);
        search.start();
        search.iterate(&s, [None, None], 4000);
        let before = search.result();
        let ours = before.acts[me].iter().max_by_key(|a| a.1).unwrap().0;
        let theirs = before.acts[1 - me].iter().max_by_key(|a| a.1).unwrap().0;
        let mut turns = [ours; 2];
        turns[1 - me] = theirs;
        let mut next = s;
        e.step(&mut next, &turns);
        let carried_before = search.reroot(&s, me, &ours, &next).map(|t| t.visits()).unwrap_or(0);

        assert!(search.ponder(&s, me, &ours, 3000));
        let after = search.result();
        for (b, a) in before.acts[me].iter().zip(after.acts[me].iter()) {
            assert_eq!(b.0, a.0);
            if a.0 == ours {
                assert_eq!(a.1, b.1 + 3000, "all pondering goes under our turn");
            } else {
                assert_eq!(a.1, b.1, "no pondering under a turn we did not play");
            }
        }
        // the opponent's replies share the pondering as they shared the search before it
        let total: u32 = before.acts[1 - me].iter().map(|a| a.1).sum();
        for (b, a) in before.acts[1 - me].iter().zip(after.acts[1 - me].iter()) {
            let want = 3000.0 * b.1 as f64 / total as f64;
            let got = (a.1 - b.1) as f64;
            assert!((got - want).abs() <= 1.0, "a reply with {} of {total} visits got {got} of 3000, wanted {want:.1}", b.1);
        }
        let carried_after = search.reroot(&s, me, &ours, &next).map(|t| t.visits()).unwrap_or(0);
        assert!(carried_after > carried_before, "the branch the game goes into grew: {carried_before} to {carried_after}");

        // a turn that is not in the root's list is refused and changes nothing
        let mut odd = victris_engine::turn::RawTurn::step(0);
        odd.dash = victris_engine::turn::RawTurn::dash(0).dash;
        if !after.acts[me].iter().any(|a| a.0 == odd) {
            assert!(!search.ponder(&s, me, &odd, 100));
        }
    }
}
