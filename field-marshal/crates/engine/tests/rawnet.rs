//! The fast raw-board judgment (`RawNet::value0`) must give exactly what the plain one gives.

use victris_engine::rawnet::RawNet;
use victris_engine::Engine;

#[test]
fn fast_value_matches_the_reference_bit_for_bit() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/raw/columns-21.json");
    let net = RawNet::load(&path).expect("models/raw/columns-21.json");
    let mut checked = 0;
    for map in victris_engine::shipped::MAP_NAMES {
        let e = Engine::shipped(map);
        for seed in 1..=6i64 {
            let mut s = e.new_match(seed);
            let mut rng = (seed as u64).wrapping_mul(0x9e3779b97f4a7c15) | 1;
            while s.result.is_none() {
                let (fast, plain) = (net.value0(&e, &s), net.value0_ref(&e, &s));
                assert_eq!(fast.to_bits(), plain.to_bits(), "{map} seed {seed} tick {}: {fast} against {plain}", s.tick);
                checked += 1;
                let turns = [victris_engine::clone::clone_turn(&e, &s, 0, &mut rng), victris_engine::clone::clone_turn(&e, &s, 1, &mut rng)];
                e.step(&mut s, &turns);
            }
        }
    }
    assert!(checked > 1000, "only {checked} positions checked");
}
