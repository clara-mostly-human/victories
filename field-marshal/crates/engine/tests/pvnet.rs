//! The learning network's arithmetic: the slopes the trainer follows are the true slopes of its
//! loss, saved positions read back as written, every turn the search may play has its own kind,
//! and the search keeps its tree from one tick to the next.

use victris_engine::pvnet::{class_of, layout, loss_grad, turns, PvNet, Rec, Scratch, CLASSES, REC};
use victris_engine::pvsearch::{pick, record, PvConfig, Rng, Search};
use victris_engine::{Engine, RawTurn};

fn random_net(seed: u64) -> PvNet {
    let mut net = PvNet::blank("columns-21", 24, 12, seed);
    let mut rng = Rng::new(seed + 7);
    for w in net.w.iter_mut() {
        *w += (rng.unit() as f32 - 0.5) * 0.3;
    }
    net
}

/// Plays a short random game and returns saved positions from it, with made-up visits.
fn positions(e: &Engine, seed: u64, want: usize) -> Vec<Rec> {
    let mut rng = Rng::new(seed);
    let mut s = e.new_match(seed as i64);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while s.result.is_none() && out.len() < want {
        let mut pair = [RawTurn::IDLE; 2];
        let mut visits: [Vec<(RawTurn, u32)>; 2] = [Vec::new(), Vec::new()];
        for x in 0..2 {
            turns(e, &s, x, &mut buf);
            visits[x] = buf.iter().map(|t| (*t, 1 + rng.below(40) as u32)).collect();
            pair[x] = pick(&visits[x], 1.0, &mut rng);
        }
        if s.tick % 3 == 0 {
            let mut r = record(e, &s, [&visits[0], &visits[1]], Some(rng.unit() as f32 - 0.5));
            r.z = (rng.below(3) as i8) - 1;
            out.push(r);
        }
        e.step(&mut s, &pair);
    }
    out
}

#[test]
fn every_turn_has_its_own_kind() {
    let e = Engine::shipped("columns-21");
    let mut rng = Rng::new(3);
    let mut s = e.new_match(5);
    let mut buf = Vec::new();
    let mut seen_kinds = [false; CLASSES];
    while s.result.is_none() && s.tick < 300 {
        let mut pair = [RawTurn::IDLE; 2];
        for x in 0..2 {
            turns(&e, &s, x, &mut buf);
            let mut used = [false; CLASSES];
            for t in &buf {
                let c = class_of(t);
                assert!(!used[c], "two turns of kind {c} at tick {}", s.tick);
                used[c] = true;
                seen_kinds[c] = true;
            }
            pair[x] = buf[rng.below(buf.len())];
        }
        e.step(&mut s, &pair);
    }
    let kinds = seen_kinds.iter().filter(|k| **k).count();
    assert!(kinds >= 17, "the game offered only {kinds} kinds of turn");
}

#[test]
fn saved_positions_read_back_as_written() {
    let e = Engine::shipped("columns-21");
    for r in positions(&e, 11, 20) {
        let mut bytes = Vec::new();
        r.write(&mut bytes);
        assert_eq!(bytes.len(), REC);
        let back = Rec::read(&bytes);
        assert_eq!(back.a, r.a);
        assert_eq!(back.b, r.b);
        assert_eq!(back.pol, r.pol);
        assert_eq!(back.legal, r.legal);
        assert_eq!(back.z, r.z);
        assert_eq!(back.has_q, r.has_q);
        assert_eq!(back.tick, r.tick);
        assert!((back.q - r.q).abs() < 0.01);
    }
}

#[test]
fn the_trainers_slopes_are_the_true_slopes() {
    let e = Engine::shipped("columns-21");
    let mut net = random_net(5);
    let recs = positions(&e, 23, 12);
    assert!(recs.len() >= 8);
    let (lambda, pw) = (0.5f32, 1.0f32);
    let len = layout(net.h1, net.h2).len;
    let mut grad = vec![0.0f32; len];
    let mut sc = Scratch::default();
    let total = |net: &PvNet, sc: &mut Scratch| -> f64 { recs.iter().map(|r| { let l = loss_grad(net, r, lambda, pw, sc, None); (l.value + pw * l.policy) as f64 }).sum() };
    for r in &recs {
        loss_grad(&net, r, lambda, pw, &mut sc, Some(&mut grad));
    }
    let mut rng = Rng::new(99);
    let mut checked = 0;
    let mut worst = 0.0f64;
    for _ in 0..1500 {
        let k = rng.below(len);
        let was = net.w[k];
        let h = 2e-3f32;
        net.w[k] = was + h;
        let up = total(&net, &mut sc);
        net.w[k] = was - h;
        let down = total(&net, &mut sc);
        net.w[k] = was;
        let numeric = (up - down) / (2.0 * h as f64);
        let exact = grad[k] as f64;
        // the loss is summed in single precision, so the numeric slope is only good to about 1e-3
        if numeric.abs() < 0.02 && exact.abs() < 0.02 {
            assert!((numeric - exact).abs() < 4e-3, "weight {k}: numeric slope {numeric}, the trainer's {exact}");
            continue;
        }
        let off = (numeric - exact).abs() / numeric.abs().max(exact.abs());
        worst = worst.max(off);
        checked += 1;
        assert!(off < 0.08, "weight {k}: numeric slope {numeric}, the trainer's {exact}");
    }
    assert!(checked > 100, "only {checked} weights had a slope to check");
    println!("checked {checked} weights, worst relative gap {worst:.4}");
}

#[test]
fn the_search_keeps_its_tree_between_ticks() {
    let e = Engine::shipped("columns-21");
    let net = random_net(8);
    let mut s = e.new_match(2);
    let mut search = Search::new(&e, &net, PvConfig::default());
    let mut kept_any = false;
    for _ in 0..40 {
        if s.result.is_some() {
            break;
        }
        search.iterate(&s, 300);
        let root = search.root();
        for x in 0..2 {
            let visits: u32 = root[x].iter().map(|a| a.1).sum();
            assert!(visits >= 300, "fighter {x}: {visits} visits after 300 lines");
        }
        assert!(search.value().abs() <= 1.0);
        let pair = [search.best(0), search.best(1)];
        let before = search.nodes();
        e.step(&mut s, &pair);
        if search.advance(&pair[0], &pair[1]) {
            kept_any = true;
            assert!(search.nodes() < before);
            // what was kept is the tree under the new root: its counts must add up
            let root = search.root();
            let (n0, n1): (u32, u32) = (root[0].iter().map(|a| a.1).sum(), root[1].iter().map(|a| a.1).sum());
            assert_eq!(n0, n1);
        }
    }
    assert!(kept_any, "the most visited pair of turns should have been searched at least once");
}

/// The state mirrored (bit 0 of `k`: left to right, bit 1: top to bottom), built by hand from the
/// engine's own fields, to check the trainer's mirror images against.
fn mirrored(e: &Engine, s: &victris_engine::State, k: usize) -> victris_engine::State {
    let flip = |p: [i32; 2]| [if k & 1 != 0 { e.map.w - 1 - p[0] } else { p[0] }, if k & 2 != 0 { e.map.h - 1 - p[1] } else { p[1] }];
    let mut t = *s;
    for f in t.fighters.iter_mut() {
        f.pos = flip(f.pos);
    }
    for i in 0..t.n_projectiles as usize {
        t.projectiles[i].pos = flip(t.projectiles[i].pos);
    }
    for i in 0..t.n_areas as usize {
        t.areas[i].pos = flip(t.areas[i].pos);
    }
    for (i, pd) in e.map.pads.iter().enumerate() {
        let j = e.map.pads.iter().position(|q| flip(q.pos) == pd.pos).expect("the map's pads mirror onto each other");
        t.pads[i] = s.pads[j];
    }
    t
}

fn mirrored_turn(k: usize, t: &RawTurn) -> RawTurn {
    use victris_engine::geom::DIR_VEC;
    use victris_engine::turn::DirIn;
    let dir = |d: DirIn| match d {
        DirIn::Dir(d) => {
            let v = DIR_VEC[d as usize];
            let m = [if k & 1 != 0 { -v[0] } else { v[0] }, if k & 2 != 0 { -v[1] } else { v[1] }];
            DirIn::Dir(DIR_VEC.iter().position(|u| *u == m).unwrap() as u8)
        }
        other => other,
    };
    RawTurn { mv: dir(t.mv), dash: dir(t.dash), ..*t }
}

#[test]
fn a_mirror_image_of_a_saved_position_is_the_mirrored_position_saved() {
    use victris_engine::pvnet::Sym;
    use victris_engine::rawnet::OWN;
    let e = Engine::shipped("columns-21");
    let sym = Sym::of(&e);
    assert_eq!(sym.ok, [true; 4], "columns-21 mirrors both ways");
    let (mut checked, mut sight_differs) = (0u32, 0u32);
    for seed in 0..6u64 {
        let mut rng = Rng::new(seed + 40);
        let mut s = e.new_match(seed as i64);
        let mut buf = Vec::new();
        while s.result.is_none() && s.tick < 400 {
            let mut pair = [RawTurn::IDLE; 2];
            let mut visits: [Vec<(RawTurn, u32)>; 2] = [Vec::new(), Vec::new()];
            for x in 0..2 {
                turns(&e, &s, x, &mut buf);
                visits[x] = buf.iter().map(|t| (*t, 1 + rng.below(40) as u32)).collect();
                pair[x] = pick(&visits[x], 1.0, &mut rng);
            }
            let here = record(&e, &s, [&visits[0], &visits[1]], Some(0.25));
            for k in 1..4 {
                let image = sym.image(k, &here);
                let there_visits: [Vec<(RawTurn, u32)>; 2] = [visits[0].iter().map(|v| (mirrored_turn(k, &v.0), v.1)).collect(), visits[1].iter().map(|v| (mirrored_turn(k, &v.0), v.1)).collect()];
                let there = record(&e, &mirrored(&e, &s, k), [&there_visits[0], &there_visits[1]], Some(0.25));
                checked += 1;
                // line of sight is the engine's own walk from tile to tile; if it is not the same
                // mirrored, the two positions really differ, and so may the attacks on offer
                if image.a[OWN - 1] != there.a[OWN - 1] || image.a[2 * OWN - 1] != there.a[2 * OWN - 1] {
                    sight_differs += 1;
                    continue;
                }
                assert_eq!(image.a, there.a, "seed {seed} tick {} image {k}: fighter 0's view", s.tick);
                assert_eq!(image.b, there.b, "seed {seed} tick {} image {k}: fighter 1's view", s.tick);
                assert_eq!(image.legal, there.legal, "seed {seed} tick {} image {k}: turns on offer", s.tick);
                assert_eq!(image.pol, there.pol, "seed {seed} tick {} image {k}: visit shares", s.tick);
            }
            e.step(&mut s, &pair);
        }
    }
    println!("{checked} images checked, line of sight differed in {sight_differs}");
    assert!(checked > 1000 && sight_differs * 50 < checked);
}
