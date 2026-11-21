//! The learning network's arithmetic: the slopes the trainer follows are the true slopes of its
//! loss, saved games replay to the positions they were played from, every turn the search may
//! play has its own kind, the map's mirror images are the same game, and the search keeps its
//! tree from one tick to the next.

use victris_engine::azdata::{shares, shares_earned, GameLog, TickLog};
use victris_engine::pvnet::{class_of, encode, layout, loss_grad, turns, Inputs, PvNet, Sample, Scratch, Sym, CLASSES, IN};
use victris_engine::pvsearch::{pick, PvConfig, Rng, Search};
use victris_engine::{Engine, RawTurn};

fn random_net(seed: u64) -> PvNet {
    let mut net = PvNet::blank("columns-21", 24, 12, seed);
    let mut rng = Rng::new(seed + 7);
    for w in net.w.iter_mut() {
        *w += (rng.unit() as f32 - 0.5) * 0.5;
    }
    net
}

/// Plays a random game and returns it as it would be saved, with made-up visits.
fn random_game(e: &Engine, seed: u64, max_ticks: i32) -> GameLog {
    let mut rng = Rng::new(seed);
    let mut s = e.new_match(seed as i64);
    let mut log = GameLog { seed: seed as i64, z: 0, ticks: Vec::new() };
    let mut buf = Vec::new();
    while s.result.is_none() && s.tick < max_ticks {
        let mut pair = [RawTurn::IDLE; 2];
        let mut visits: [Vec<(RawTurn, u32)>; 2] = [Vec::new(), Vec::new()];
        for x in 0..2 {
            turns(e, &s, x, &mut buf);
            visits[x] = buf.iter().map(|t| (*t, 1 + rng.below(40) as u32)).collect();
            pair[x] = pick(&visits[x], 1.0, &mut rng);
        }
        log.ticks.push(TickLog { turns: pair, q: Some(rng.unit() as f32 - 0.5), pol: Some([shares(&visits[0]), shares(&visits[1])]) });
        e.step(&mut s, &pair);
    }
    // play on with idle turns to the end, as unsearched ticks
    while s.result.is_none() {
        log.ticks.push(TickLog { turns: [RawTurn::IDLE; 2], q: None, pol: None });
        e.step(&mut s, &[RawTurn::IDLE; 2]);
    }
    log.z = victris_engine::pvsearch::outcome(&s).unwrap() as i8;
    log
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
fn saved_games_replay_to_the_positions_they_were_played_from() {
    let e = Engine::shipped("columns-21");
    let mut bytes = Vec::new();
    let games: Vec<GameLog> = (0..4).map(|k| random_game(&e, 11 + k, 150)).collect();
    for g in &games {
        g.write(&mut bytes);
    }
    let back = GameLog::read_all(&bytes);
    assert_eq!(back.len(), games.len());
    for (a, b) in games.iter().zip(&back) {
        assert_eq!((a.seed, a.z, a.ticks.len()), (b.seed, b.z, b.ticks.len()));
        for (x, y) in a.ticks.iter().zip(&b.ticks) {
            assert_eq!(x.turns, y.turns);
            assert_eq!(x.pol, y.pol);
            assert_eq!(x.q.is_some(), y.q.is_some());
            assert!((x.q.unwrap_or(0.0) - y.q.unwrap_or(0.0)).abs() < 0.01);
        }
        let mut samples = Vec::new();
        assert!(b.samples(&e, &mut samples), "the saved game replays to its own result");
        assert_eq!(samples.len(), a.ticks.iter().filter(|t| t.pol.is_some()).count());
        // the positions are the ones the game went through
        let mut s = e.new_match(a.seed);
        for (k, t) in a.ticks.iter().take(samples.len()).enumerate() {
            assert_eq!(samples[k].s, s);
            e.step(&mut s, &t.turns);
        }
    }
    // a game cut off mid-file is left out, the ones before it are read
    assert_eq!(GameLog::read_all(&bytes[..bytes.len() - 5]).len(), games.len() - 1);
    // a game whose result does not match its replay is refused
    let mut wrong = back[0].clone();
    wrong.z = if wrong.z == 1 { -1 } else { 1 };
    assert!(!wrong.samples(&e, &mut Vec::new()));
}

#[test]
fn the_inputs_stay_in_range_and_few() {
    let e = Engine::shipped("columns-21");
    let mut samples = Vec::new();
    for seed in 0..4 {
        assert!(random_game(&e, 60 + seed, 400).samples(&e, &mut samples));
    }
    let mut x = Inputs::default();
    let mut most = 0;
    for r in &samples {
        for f in 0..2 {
            encode(&e, &r.s, f, &mut x);
            most = most.max(x.n);
            let mut seen = vec![false; IN];
            for k in 0..x.n {
                let i = x.idx[k] as usize;
                assert!(i < IN && !seen[i], "input {i} given twice or out of range");
                seen[i] = true;
                assert!(x.val[k].abs() <= 1.5, "input {i} = {}", x.val[k]);
            }
        }
    }
    println!("{} positions, at most {most} of {IN} inputs set", samples.len());
    assert!(most < 150);
}

#[test]
fn the_trainers_slopes_are_the_true_slopes() {
    let e = Engine::shipped("columns-21");
    let mut net = random_net(5);
    let mut all = Vec::new();
    assert!(random_game(&e, 23, 200).samples(&e, &mut all));
    let recs: Vec<Sample> = all.into_iter().step_by(9).take(16).collect();
    assert!(recs.len() >= 8);
    let (lambda, pw) = (0.5f32, 1.0f32);
    let len = layout(net.h1, net.h2).len;
    let mut grad = vec![0.0f32; len];
    let mut sc = Scratch::default();
    let total = |net: &PvNet, sc: &mut Scratch| -> f64 { recs.iter().map(|r| { let l = loss_grad(net, &e, r, lambda, pw, sc, None); (l.value + pw * l.policy) as f64 }).sum() };
    for r in &recs {
        loss_grad(&net, &e, r, lambda, pw, &mut sc, Some(&mut grad));
    }
    // the weights with the steepest slopes: the loss is summed in single precision, so a numeric
    // slope is only good to about 1e-3, and only steep ones can be compared closely
    let mut order: Vec<usize> = (0..len).collect();
    order.sort_by(|a, b| grad[*b].abs().partial_cmp(&grad[*a].abs()).unwrap());
    let mut rng = Rng::new(99);
    let mut picks: Vec<usize> = order[..300].to_vec();
    picks.extend((0..300).map(|_| rng.below(len)));
    let (mut checked, mut worst) = (0, 0.0f64);
    for k in picks {
        let was = net.w[k];
        let h = 2e-3f32;
        net.w[k] = was + h;
        let up = total(&net, &mut sc);
        net.w[k] = was - h;
        let down = total(&net, &mut sc);
        net.w[k] = was;
        let numeric = (up - down) / (2.0 * h as f64);
        let exact = grad[k] as f64;
        if numeric.abs() < 0.05 && exact.abs() < 0.05 {
            assert!((numeric - exact).abs() < 5e-3, "weight {k}: numeric slope {numeric}, the trainer's {exact}");
            continue;
        }
        let off = (numeric - exact).abs() / numeric.abs().max(exact.abs());
        worst = worst.max(off);
        checked += 1;
        assert!(off < 0.05, "weight {k}: numeric slope {numeric}, the trainer's {exact}");
    }
    println!("checked {checked} steep weights, worst relative gap {worst:.4}");
    assert!(checked > 100, "only {checked} weights had a slope steep enough to check");
}

#[test]
fn a_mirror_image_is_the_same_game() {
    let e = Engine::shipped("columns-21");
    let sym = Sym::of(&e);
    assert_eq!(sym.ok, [true; 4], "columns-21 mirrors both ways");
    let (mut steps, mut differs) = (0u32, 0u32);
    let mut x = Inputs::default();
    for seed in 0..6u64 {
        let mut rng = Rng::new(seed + 40);
        let mut s = e.new_match(seed as i64);
        let mut buf = Vec::new();
        while s.result.is_none() && s.tick < 400 {
            let mut pair = [RawTurn::IDLE; 2];
            for f in 0..2 {
                turns(&e, &s, f, &mut buf);
                pair[f] = buf[rng.below(buf.len())];
            }
            let mut next = s;
            e.step(&mut next, &pair);
            for k in 1..4 {
                let image = sym.state(k, &s);
                // mirrored twice is the position itself
                assert_eq!(sym.state(k, &image), s);
                // the same turns are on offer, mirrored
                for f in 0..2 {
                    turns(&e, &s, f, &mut buf);
                    let mut here: Vec<usize> = buf.iter().map(|t| sym.class(k, class_of(t))).collect();
                    assert!(buf.iter().all(|t| class_of(&sym.turn(k, t)) == sym.class(k, class_of(t))));
                    turns(&e, &image, f, &mut buf);
                    let mut there: Vec<usize> = buf.iter().map(class_of).collect();
                    here.sort();
                    there.sort();
                    assert_eq!(here, there, "seed {seed} tick {} image {k} fighter {f}", s.tick);
                    // and the fighter sees as much of the board in the image as here
                    encode(&e, &s, f, &mut x);
                    let n = x.n;
                    encode(&e, &image, f, &mut x);
                    assert_eq!(n, x.n, "seed {seed} tick {} image {k} fighter {f}: inputs set", s.tick);
                }
                // playing the mirrored turns in the image gives the image of what happened here
                let mut image_next = image;
                e.step(&mut image_next, &[sym.turn(k, &pair[0]), sym.turn(k, &pair[1])]);
                steps += 1;
                if image_next != sym.state(k, &next) {
                    differs += 1;
                }
            }
            s = next;
        }
    }
    println!("{steps} mirrored steps, {differs} came out different from the mirrored result");
    assert!(steps > 1000 && differs * 20 < steps, "{differs} of {steps} mirrored steps differ");
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
        let old = s;
        e.step(&mut s, &pair);
        // every other tick the search is told only where the game went, as a live bot is
        let kept = if s.tick % 2 == 0 {
            search.advance(&pair[0], &pair[1])
        } else {
            // the way a bot keeps it: the tree alone is carried over, and a new search takes it up
            let tree = search.into_tree();
            search = Search::new(&e, &net, PvConfig::default());
            search.start_from(tree);
            search.advance_to(&old, &s)
        };
        if kept {
            kept_any = true;
            assert!(search.nodes() < before);
            // what was kept is the tree under the new root: its counts must add up
            let root = search.root();
            let (n0, n1): (u32, u32) = (root[0].iter().map(|a| a.1).sum(), root[1].iter().map(|a| a.1).sum());
            assert_eq!(n0, n1);
            assert_eq!(n0, search.visits());
        }
    }
    assert!(kept_any, "the most visited pair of turns should have been searched at least once");
}

#[test]
fn pondering_fills_the_tree_under_the_turn_already_sent() {
    let e = Engine::shipped("columns-21");
    let net = random_net(11);
    let mut s = e.new_match(3);
    let (mut plain_kept, mut ponder_kept, mut ticks) = (0u64, 0u64, 0u32);
    for _ in 0..30 {
        if s.result.is_some() {
            break;
        }
        // two searches of the same position; one also ponders under fighter 0's turn
        let mut plain = Search::new(&e, &net, PvConfig::default());
        plain.iterate(&s, 400);
        let mut pondered = Search::new(&e, &net, PvConfig::default());
        pondered.iterate(&s, 400);
        let ours = pondered.best(0);
        assert_eq!(ours, plain.best(0), "the same search twice gives the same turn");
        assert!(pondered.ponder(&s, 0, &ours, 400));
        // every pondered line began with our turn, and the counts still add up
        let root = pondered.root();
        let mine = root[0].iter().find(|a| a.0 == ours).unwrap().1;
        let plain_mine = plain.root()[0].iter().find(|a| a.0 == ours).unwrap().1;
        assert_eq!(mine, plain_mine + 400);
        assert_eq!(root[0].iter().map(|a| a.1).sum::<u32>(), 800);
        assert_eq!(root[1].iter().map(|a| a.1).sum::<u32>(), 800);
        // the opponent's turns keep their shares: none gains more than its share of 400 plus one
        for (a, b) in plain.root()[1].iter().zip(root[1].iter()) {
            let share = a.1 as f64 / 400.0 * 400.0;
            assert!((b.1 - a.1) as f64 <= share + 1.0, "an opponent turn with {} visits got {} more", a.1, b.1 - a.1);
        }
        // the game moves on with our turn and the opponent's most visited one
        let pair = [ours, plain.best(1)];
        let old = s;
        e.step(&mut s, &pair);
        if s.result.is_some() {
            break;
        }
        if plain.advance_to(&old, &s) && pondered.advance_to(&old, &s) {
            assert!(pondered.visits() >= plain.visits());
            plain_kept += plain.visits() as u64;
            ponder_kept += pondered.visits() as u64;
            ticks += 1;
        }
    }
    assert!(ticks > 5, "the tree should carry over on most ticks");
    assert!(ponder_kept as f64 > 1.5 * plain_kept as f64, "pondering kept {ponder_kept} visits against {plain_kept} without");
    // a turn that is not at the root is refused
    let mut search = Search::new(&e, &net, PvConfig::default());
    assert!(!search.ponder(&s, 0, &RawTurn::IDLE, 10), "nothing searched yet");
}

#[test]
fn earned_visits_leave_out_what_the_wide_root_forced() {
    let e = Engine::shipped("columns-21");
    let net = random_net(11);
    let mut s = e.new_match(3);
    // a few ticks in, so both fighters have a choice of turns
    for _ in 0..6 {
        e.step(&mut s, &[RawTurn::IDLE, RawTurn::IDLE]);
    }
    let mut wide = Search::new(&e, &net, PvConfig { even_root: 0.25, ..Default::default() });
    wide.iterate(&s, 800);
    let (plain, earned) = (wide.root(), wide.root_earned());
    for x in 0..2 {
        assert_eq!(plain[x].len(), earned[x].len());
        let top = (0..plain[x].len()).max_by_key(|i| plain[x][*i].1).unwrap();
        let mut kept = 0.0f32;
        for i in 0..plain[x].len() {
            assert_eq!(plain[x][i].0, earned[x][i].0);
            assert!(earned[x][i].1 >= 0.0 && earned[x][i].1 <= plain[x][i].1 as f32, "fighter {x} turn {i}: earned {} of {}", earned[x][i].1, plain[x][i].1);
            kept += earned[x][i].1;
        }
        // the most visited turn keeps every visit; the width's visits are gone from the others
        assert_eq!(earned[x][top].1, plain[x][top].1 as f32);
        let all: u32 = plain[x].iter().map(|a| a.1).sum();
        assert!(kept < all as f32, "fighter {x}: nothing was taken out ({kept} of {all})");
        // saved as shares: they add up to the whole, and fewer kinds of turn carry any
        let (a, b) = (shares(&plain[x].iter().map(|v| (v.0, v.1)).collect::<Vec<_>>()), shares_earned(&earned[x]));
        let total: u32 = b.iter().map(|v| *v as u32).sum();
        assert!((230..=290).contains(&total), "fighter {x}: shares add up to {total}");
        assert!(b.iter().filter(|v| **v > 0).count() <= a.iter().filter(|v| **v > 0).count());
        for c in 0..CLASSES {
            assert!(a[c] > 0 || b[c] == 0, "a kind of turn with no visits has earned some");
        }
    }
    // before any line is searched there is nothing to earn
    let fresh = Search::new(&e, &net, PvConfig::default());
    assert!(fresh.root_earned().iter().all(|l| l.iter().all(|v| v.1 == 0.0)));
}
