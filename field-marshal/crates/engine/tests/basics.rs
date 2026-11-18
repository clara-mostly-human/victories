use victris_engine::turn::RawTurn;
use victris_engine::{EndReason, Engine};

/// The hash of the shipped rules table as every ladder replay and `hello` reports it.
/// This checks canonical JSON and cyrb53 against a value the server computed.
#[test]
fn shipped_rules_hash() {
    let e = Engine::shipped("arena-21");
    assert_eq!(e.rules.version, "0.22.1");
    assert_eq!(e.rules.hash, "1f7e424f31c00f");
}

/// Two idle fighters: the match ends on the last tick the rules allow, or earlier if the
/// closing zone kills both on the same tick.
#[test]
fn idle_match_ends_by_zone_or_tick_limit() {
    for name in victris_engine::shipped::MAP_NAMES {
        let e = Engine::shipped(name);
        let mut s = e.new_match(42);
        while s.result.is_none() {
            e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
        }
        let r = s.result.unwrap();
        assert_eq!(r.final_tick, s.tick - 1);
        assert!(s.tick <= e.rules.max_ticks);
        // The spawns mirror each other, so two idle fighters take the same zone damage.
        assert!(matches!(r.reason, EndReason::TimeoutDraw | EndReason::AllDead), "{name}: {r:?}");
        assert_eq!(s.fighters[0].hp, s.fighters[1].hp);
    }
}

/// The state is plain data: a copy steps to the same place as the original.
#[test]
fn copies_are_independent_and_deterministic() {
    let e = Engine::shipped("columns-21");
    let ranged = e.rules.attack_index("ranged").unwrap();
    let mut a = e.new_match(7);
    for t in 0..40 {
        let turns = [
            RawTurn::step((t % 8) as u8).with_attack(ranged, a.fighters[1].pos),
            RawTurn::step(((t + 3) % 8) as u8).with_attack(ranged, a.fighters[0].pos),
        ];
        let mut b = a;
        e.step(&mut a, &turns);
        e.step(&mut b, &turns);
        assert_eq!(a, b);
        assert_eq!(e.state_hash(&a), e.state_hash(&b));
        if a.result.is_some() {
            break;
        }
    }
}
