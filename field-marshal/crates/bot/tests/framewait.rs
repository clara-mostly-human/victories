//! The think cap after a frame waited behind others (a burst after a stall on the
//! connection): the newest frame gets what is left of the think time, at least 20 ms, and a
//! frame with a newer observation behind it gets none.

use marshal::tracker::Tracker;
use victris_engine::Engine;

#[test]
fn a_waiting_frame_thinks_less_and_a_stale_one_not_at_all() {
    let t = Tracker::for_engine(Engine::shipped("columns-21"), 0, 1);
    let full = t.think_cap_ms(450);
    assert_eq!(full, 450, "the cap at the start of a match is the playbook's think time");
    t.note_frame_wait(200);
    assert_eq!(t.think_cap_ms(450), 250);
    t.note_frame_wait(440);
    assert_eq!(t.think_cap_ms(450), 20, "at least 20 ms, so the search still runs");
    t.note_frame_wait(u64::MAX);
    assert_eq!(t.think_cap_ms(450), 0, "a newer observation is queued: answer at once");
    t.note_frame_wait(0);
    assert_eq!(t.think_cap_ms(450), full);
}

#[test]
fn a_slow_network_cuts_the_think_and_a_fast_one_gives_it_back() {
    let t = Tracker::for_engine(Engine::shipped("columns-21"), 0, 1);
    t.note_frame_wait(0);
    t.note_wire_worst(100);
    assert_eq!(t.think_cap_ms(450), 450, "a normal trip leaves the full think");
    assert_eq!(t.net_cap_ms.get(), 0);
    t.note_wire_worst(550);
    assert_eq!(t.think_cap_ms(450), 350, "think plus the worst trip stays under 900 ms");
    assert_eq!(t.net_cap_ms.get(), 350);
    t.note_wire_worst(1000);
    assert_eq!(t.think_cap_ms(450), 100, "never below 100 ms, so the search still runs");
    t.note_frame_wait(200);
    assert_eq!(t.think_cap_ms(450), 20, "a waiting frame is cut from what the network leaves");
    t.note_frame_wait(0);
    assert_eq!(t.net_cap_ms.get(), 0, "each decision starts uncapped");
    t.note_wire_worst(120);
    assert_eq!(t.think_cap_ms(450), 450, "back to the full think once the trips are short");
}
