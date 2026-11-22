//! The learning network's arithmetic: the slopes the trainer follows are the true slopes of its
//! loss, saved games replay to the positions they were played from, every turn the search may
//! play has its own kind, the map's mirror images are the same game, and the search keeps its
//! tree from one tick to the next. Positions beside a game's path are saved with the game and
//! teach their value, and looking at them leaves the game as it was.

use victris_engine::azdata::{shares, shares_earned, GameLog, SideLog, TickLog};
use victris_engine::pvnet::{act, class_of, encode, layout, loss_grad, turns, Inputs, Pass, PvNet, Sample, Scratch, Sym, CLASSES, IN, IN_OLD};
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
        log.ticks.push(TickLog { turns: pair, q: Some(rng.unit() as f32 - 0.5), pol: Some([shares(&visits[0]), shares(&visits[1])]), side: Vec::new() });
        e.step(&mut s, &pair);
    }
    // play on with idle turns to the end, as unsearched ticks
    while s.result.is_none() {
        log.ticks.push(TickLog { turns: [RawTurn::IDLE; 2], q: None, pol: None, side: Vec::new() });
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
    let len = net.layout().len;
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

#[test]
fn a_lesson_without_shares_teaches_the_value_and_leaves_the_moves_alone() {
    // what azreal writes from a real game: the positions and who won, no shares for the turns
    let e = Engine::shipped("columns-21");
    let mut game = random_game(&e, 21, 60);
    for t in game.ticks.iter_mut() {
        t.q = None;
        t.pol = t.pol.map(|_| [[0u8; CLASSES]; 2]);
    }
    // it is saved and read back as it was
    let mut bytes = Vec::new();
    game.write(&mut bytes);
    let back = GameLog::read_all(&bytes);
    assert_eq!(back.len(), 1);
    let mut samples: Vec<Sample> = Vec::new();
    assert!(back[0].samples(&e, &mut samples));
    assert!(samples.len() >= 30, "{} positions", samples.len());
    let net = random_net(3);
    let n = net.layout().len;
    let mut sc = Scratch::default();
    let mut moved_value = false;
    for r in &samples {
        // the lesson is the game's result, nothing from a search
        assert!(!r.has_q && r.z == game.z);
        assert!(r.pol.iter().all(|f| f.iter().all(|v| *v == 0)));
        let mut grad = vec![0.0f32; n];
        let loss = loss_grad(&net, &e, r, 0.5, 1.0, &mut sc, Some(&mut grad));
        // no moves loss at all, and a value loss with a slope
        assert_eq!(loss.policy_n, 0);
        assert_eq!(loss.policy, 0.0);
        assert!(loss.value > 0.0);
        moved_value |= grad.iter().any(|g| *g != 0.0);
        // the same position with shares does carry a moves loss: the lesson above left it out,
        // the trainer did not lose it
        let mut with = r.clone();
        let mut buf = Vec::new();
        turns(&e, &r.s, 0, &mut buf);
        if buf.len() >= 2 {
            with.pol[0][class_of(&buf[0])] = 255;
            assert_eq!(loss_grad(&net, &e, &with, 0.5, 1.0, &mut sc, None).policy_n, 1);
        }
    }
    assert!(moved_value, "the value was not taught");
}

#[test]
fn weighing_compares_our_turns_against_what_is_expected_and_their_best_answer() {
    let e = Engine::shipped("columns-21");
    let net = random_net(17);
    let mut s = e.new_match(5);
    let mut weighed_ticks = 0;
    for _ in 0..40 {
        if s.result.is_some() {
            break;
        }
        let mut search = Search::new(&e, &net, PvConfig::default());
        search.iterate(&s, 600);
        let before = search.root();
        let total: u32 = before[0].iter().map(|a| a.1).sum();
        let w = search.weigh(&s, 0, 400, 0.5);
        assert!(!w.is_empty() && w.len() <= 8);
        if w.len() > 1 {
            weighed_ticks += 1;
            // only turns the search took seriously are weighed, and no turn twice
            for (k, a) in w.iter().enumerate() {
                let seen = before[0].iter().find(|b| b.0 == a.turn).expect("a weighed turn is one of the root's");
                assert!(seen.1 as u64 * 50 >= total as u64, "a turn with {} of {} visits was weighed", seen.1, total);
                assert!(w[..k].iter().all(|b| b.turn != a.turn));
                // what it is worth lies between its expected result and its result against their answer
                let (lo, hi) = (a.expected.min(a.worst), a.expected.max(a.worst));
                assert!(a.worth >= lo - 1e-4 && a.worth <= hi + 1e-4, "worth {} outside {} to {}", a.worth, lo, hi);
                assert!((-1.0..=1.0).contains(&a.expected) && (-1.0..=1.0).contains(&a.worst));
                // their answer is one turn, the same against every turn of ours
                assert_eq!(a.worst_to, w[0].worst_to);
            }
            // the most visited turn is always among them
            let top = before[0].iter().max_by_key(|a| a.1).unwrap().0;
            assert!(w.iter().any(|a| a.turn == top));
            // the turn to play is the search's own choice, unless another did better against the mix
            assert!(w[0].turn == top || w[0].worth > w.iter().find(|a| a.turn == top).unwrap().worth);
            // it spent lines, and no more than it was given
            let after: u32 = search.root()[0].iter().map(|a| a.1).sum();
            assert!(after > total && after <= total + 400, "{} lines spent of 400", after - total);
            // the same search weighed again the same way gives the same turn
            let mut again = Search::new(&e, &net, PvConfig::default());
            again.iterate(&s, 600);
            assert_eq!(again.weigh(&s, 0, 400, 0.5)[0].turn, w[0].turn);
            // all trust: worth is the expected result, and no answer is looked for
            let mut all = Search::new(&e, &net, PvConfig::default());
            all.iterate(&s, 600);
            assert!(all.weigh(&s, 0, 400, 1.0).iter().all(|a| (a.worth - a.expected).abs() < 1e-6 && a.worst_to == RawTurn::IDLE));
            // none: the result against their best answer alone
            let mut none = Search::new(&e, &net, PvConfig::default());
            none.iterate(&s, 600);
            let n = none.weigh(&s, 0, 400, 0.0);
            assert!(n.iter().all(|a| (a.worth - a.worst).abs() < 1e-6));
        }
        let pair = [w[0].turn, search.best(1)];
        e.step(&mut s, &pair);
    }
    assert!(weighed_ticks > 10, "most ticks give a choice to weigh");
    // nothing searched: the plain answer, no lines
    let mut empty = Search::new(&e, &net, PvConfig::default());
    assert_eq!(empty.weigh(&e.new_match(5), 0, 100, 0.5).len(), 1);
}

#[test]
fn threads_share_one_tree_and_leave_its_counts_whole() {
    let e = Engine::shipped("columns-21");
    let net = random_net(17);
    let mut s = e.new_match(5);
    for tick in 0..24 {
        if s.result.is_some() {
            break;
        }
        // one thread through the new entry is the plain search, line for line
        let mut plain = Search::new(&e, &net, PvConfig::default());
        plain.iterate(&s, 600);
        let mut one = Search::new(&e, &net, PvConfig::default());
        assert_eq!(one.iterate_threads(&s, 600, 1, None), 600);
        assert_eq!(format!("{:?}", one.root()), format!("{:?}", plain.root()));
        // three threads: every line counted once at the root and below, nothing left "on its way"
        let mut many = Search::new(&e, &net, PvConfig::default());
        assert_eq!(many.iterate_threads(&s, 600, 3, None), 600);
        let root = many.root();
        assert_eq!(many.visits(), 600);
        for x in 0..2 {
            assert_eq!(root[x].iter().map(|a| a.1).sum::<u32>(), 600);
            for a in root[x].iter() {
                assert!(a.2.abs() <= 1.0 + 1e-4, "a turn's mean result {} is outside -1..1: a line was left on its way", a.2);
            }
        }
        // what one fighter won on the lines the other lost
        let sum = |x: usize| root[x].iter().map(|a| a.1 as f64 * a.2 as f64).sum::<f64>();
        assert!((sum(0) + sum(1)).abs() < 0.05, "the two fighters' results do not cancel: {} and {}", sum(0), sum(1));
        // the positions below hold the root's lines between them (but for lines that ended the game)
        let below: u32 = many.positions(&s).iter().filter(|p| p.0 == 1).map(|p| p.1 + 1).sum();
        assert!(below <= 600, "{below} lines counted one tick down, 600 run");
        // pondering on threads: every line begins with our turn
        let ours = many.best(0);
        let before = root[0].iter().find(|a| a.0 == ours).unwrap().1;
        assert_eq!(many.ponder_threads(&s, 0, &ours, 300, 3, None), Some(300));
        let after = many.root();
        assert_eq!(after[0].iter().find(|a| a.0 == ours).unwrap().1, before + 300);
        assert_eq!(after[1].iter().map(|a| a.1).sum::<u32>(), 900);
        // lines kept short: many of them end on a position already there, or wait for a pair
        // another line is having played, and every one is still counted once
        let mut short = Search::new(&e, &net, PvConfig { depth: 2, ..PvConfig::default() });
        assert_eq!(short.iterate_threads(&s, 500, 4, None), 500);
        let r = short.root();
        assert_eq!(short.visits(), 500);
        for x in 0..2 {
            assert_eq!(r[x].iter().map(|a| a.1).sum::<u32>(), 500);
            assert!(r[x].iter().all(|a| a.2.abs() <= 1.0 + 1e-4), "a short line was left on its way");
        }
        // a time already past stops the threads at once
        let mut none = Search::new(&e, &net, PvConfig::default());
        assert!(none.iterate_threads(&s, 100_000, 3, Some(std::time::Instant::now())) < 400);
        // the tree is kept and moved on as ever
        let pair = [many.best(0), many.best(1)];
        let old = s;
        e.step(&mut s, &pair);
        if s.result.is_none() && tick % 3 == 0 {
            assert!(many.advance_to(&old, &s));
            assert!(many.visits() > 0);
            many.iterate_threads(&s, 200, 3, None);
            let r = many.root();
            assert_eq!(r[0].iter().map(|a| a.1).sum::<u32>(), many.visits());
        }
    }
}

#[test]
fn a_pass_in_blocks_is_the_plain_pass_to_the_last_bit() {
    // `PvNet::forward` works a layer through in blocks of units; a unit's sum must still be its
    // bias and then each input times its weight, added in the order the inputs come
    let e = Engine::shipped("columns-21");
    let mut rng = Rng::new(3);
    for (h1, h2) in [(64usize, 32usize), (24, 12), (37, 13), (128, 128)] {
        let mut net = PvNet::blank("columns-21", h1, h2, 5 + h1 as u64);
        for w in net.w.iter_mut() {
            *w += (rng.unit() as f32 - 0.5) * 0.7;
        }
        let l = net.layout();
        let w = &net.w;
        let game = random_game(&e, 40 + h1 as u64, 60);
        let mut s = e.new_match(game.seed);
        let (mut x, mut p) = (Inputs::default(), Pass::default());
        for t in game.ticks.iter().take(60) {
            for f in 0..2 {
                encode(&e, &s, f, &mut x);
                net.forward(&x, &mut p);
                let mut z1: Vec<f32> = w[l.b1..l.b1 + h1].to_vec();
                for k in 0..x.n {
                    for j in 0..h1 {
                        z1[j] += x.val[k] * w[l.t1 + x.idx[k] as usize * h1 + j];
                    }
                }
                let mut z2: Vec<f32> = w[l.b2..l.b2 + h2].to_vec();
                for i in 0..h1 {
                    for j in 0..h2 {
                        z2[j] += act(z1[i]) * w[l.t2 + i * h2 + j];
                    }
                }
                let mut numbers: Vec<f32> = w[l.pb..l.pb + CLASSES].to_vec();
                for j in 0..h2 {
                    for c in 0..CLASSES {
                        numbers[c] += act(z2[j]) * w[l.tp + j * CLASSES + c];
                    }
                }
                let bits = |v: &[f32]| v.iter().map(|a| a.to_bits()).collect::<Vec<u32>>();
                assert_eq!(bits(&p.z1[..h1]), bits(&z1), "first layer, {h1} and {h2} units");
                assert_eq!(bits(&p.z2[..h2]), bits(&z2), "second layer, {h1} and {h2} units");
                assert_eq!(bits(&p.logits), bits(&numbers), "turn numbers, {h1} and {h2} units");
                // the two halves of a pass, as the search takes them, are the whole pass
                let mut q = Pass::default();
                net.hidden(&x, &mut q);
                assert_eq!(q.g.to_bits(), p.g.to_bits());
                net.numbers(&mut q);
                assert_eq!(bits(&q.logits), bits(&p.logits));
            }
            e.step(&mut s, &t.turns);
            if s.result.is_some() {
                break;
            }
        }
    }
}

#[test]
fn a_position_no_line_was_played_on_from_is_searched_like_a_fresh_one() {
    // a new position gets only its value; its turn lists are made when a line is first played
    // on from it. Moving the tree onto such a position and searching must be the search a new
    // tree makes from there, line for line.
    let e = Engine::shipped("columns-21");
    let net = random_net(23);
    let mut s = e.new_match(11);
    let mut tried = 0;
    for _ in 0..30 {
        if s.result.is_some() {
            break;
        }
        let mut kept = Search::new(&e, &net, PvConfig::default());
        kept.iterate(&s, 400);
        let pair = [kept.best(0), kept.best(1)];
        // a position one tick down that was reached once and never played on from
        let leaf = kept.positions(&s).into_iter().find(|p| p.0 == 1 && p.1 == 0);
        if let Some((_, _, c)) = leaf {
            // (two pairs of turns can lead to the same position: the tree then moves to the more
            // searched one, which is not the case tested here)
            if kept.advance_to(&s, &c) && kept.visits() == 0 {
                tried += 1;
                kept.iterate(&c, 300);
                let mut fresh = Search::new(&e, &net, PvConfig::default());
                fresh.iterate(&c, 300);
                assert_eq!(format!("{:?}", kept.root()), format!("{:?}", fresh.root()));
                assert_eq!(kept.nodes(), fresh.nodes());
                // and the same on threads: its lists are made before the threads start
                let mut many = Search::new(&e, &net, PvConfig::default());
                many.iterate(&s, 400);
                assert!(many.advance_to(&s, &c));
                assert_eq!(many.iterate_threads(&c, 300, 3, None), 300);
                assert_eq!(many.root()[0].iter().map(|a| a.1).sum::<u32>(), 300);
                // weighing right after the move reads the root's lists too
                let mut moved = Search::new(&e, &net, PvConfig::default());
                moved.iterate(&s, 400);
                assert!(moved.advance_to(&s, &c));
                assert!(!moved.weigh(&c, 0, 100, 0.5).is_empty());
            }
        }
        // threads going on from a tree one thread made: the positions it left without lists get them
        let mut both = Search::new(&e, &net, PvConfig::default());
        both.iterate(&s, 300);
        assert_eq!(both.iterate_threads(&s, 300, 3, None), 300);
        assert_eq!(both.visits(), 600);
        assert_eq!(both.root()[1].iter().map(|a| a.1).sum::<u32>(), 600);
        e.step(&mut s, &pair);
    }
    assert!(tried >= 5, "only {tried} positions never played on from were found");
}

/// A network with the inputs networks had before the opponent's third ring of walls.
fn older_net(seed: u64, h1: usize, h2: usize) -> PvNet {
    let mut rng = Rng::new(seed);
    let w = (0..layout(IN_OLD, h1, h2).len).map(|_| (rng.unit() as f32 - 0.5) * 0.6).collect();
    PvNet { map: "columns-21".to_string(), n_in: IN_OLD, h1, h2, w }
}

#[test]
fn the_walls_three_tiles_from_the_opponent_are_inputs_and_older_networks_read_as_before() {
    let e = Engine::shipped("columns-21");
    let wall = |p: [i32; 2]| p[0] < 0 || p[1] < 0 || p[0] >= e.map.w || p[1] >= e.map.h || e.map.is_wall(p);
    let old = older_net(9, 24, 12);
    let wide = old.widened();
    assert_eq!((old.n_in, wide.n_in, wide.w.len()), (IN_OLD, IN, layout(IN, 24, 12).len));
    // saved and read back, each keeps its own number of inputs
    let dir = std::env::temp_dir();
    for (net, name) in [(&old, "pvnet-older.json"), (&wide, "pvnet-wider.json")] {
        let path = dir.join(format!("{}-{name}", std::process::id()));
        net.save(&path, &serde_json::json!({})).unwrap();
        let back = PvNet::load(&path).expect("the saved network does not load");
        std::fs::remove_file(&path).ok();
        assert_eq!((back.n_in, &back.w), (net.n_in, &net.w));
    }
    let (mut x, mut p) = (Inputs::default(), Pass::default());
    let (mut positions, mut ring_seen, mut far_only) = (0, 0, 0);
    for seed in 0..6 {
        let game = random_game(&e, 80 + seed, 300);
        let mut s = e.new_match(game.seed);
        for t in &game.ticks {
            for f in 0..2 {
                encode(&e, &s, f, &mut x);
                // the older inputs first and unchanged in number, the ring after them
                assert!((0..x.n0).all(|k| (x.idx[k] as usize) < IN_OLD) && (x.n0..x.n).all(|k| (x.idx[k] as usize) >= IN_OLD));
                // the ring is exactly the walls at three tiles from the opponent
                let o = s.fighters[1 - f].pos;
                let mut ring = 0;
                for wy in -3i32..=3 {
                    for wx in -3i32..=3 {
                        if wx.abs().max(wy.abs()) == 3 && wall([o[0] + wx, o[1] + wy]) {
                            ring += 1;
                        }
                    }
                }
                assert_eq!(x.n - x.n0, ring, "tick {}: {} ring inputs for {ring} walls three tiles from the opponent", s.tick, x.n - x.n0);
                if ring > 0 {
                    ring_seen += 1;
                    // a wall the push can reach that the older inputs did not show at all
                    let near = (-2i32..=2).any(|wy| (-2i32..=2).any(|wx| wall([o[0] + wx, o[1] + wy])));
                    far_only += !near as u32;
                }
                // the older network reads the position from the older inputs alone, in plain arithmetic
                let l = old.layout();
                old.forward(&x, &mut p);
                for j in 0..old.h1 {
                    let mut z = old.w[l.b1 + j];
                    for k in 0..x.n0 {
                        z += x.val[k] * old.w[l.t1 + x.idx[k] as usize * old.h1 + j];
                    }
                    assert_eq!(z.to_bits(), p.z1[j].to_bits(), "unit {j} of the older network");
                }
            }
            // made wider it still reads every position the same, to the last bit
            let (a, b) = (old.eval(&e, &s), wide.eval(&e, &s));
            assert_eq!(a.v0.to_bits(), b.v0.to_bits());
            assert!(a.logits.iter().flatten().zip(b.logits.iter().flatten()).all(|(u, v)| u == v));
            positions += 1;
            e.step(&mut s, &t.turns);
        }
    }
    println!("{positions} positions; walls three tiles from the opponent in {ring_seen} views, in {far_only} of them none nearer");
    assert!(ring_seen > 100 && far_only > 10);
    // and the wider one learns from the ring: its slope for those weights is not zero
    let mut all = Vec::new();
    assert!(random_game(&e, 81, 300).samples(&e, &mut all));
    let mut grad = vec![0.0f32; wide.layout().len];
    let mut sc = Scratch::default();
    for r in &all {
        loss_grad(&wide, &e, r, 0.5, 1.0, &mut sc, Some(&mut grad));
    }
    let l = wide.layout();
    let ring = &grad[l.t1 + IN_OLD * wide.h1..l.b1];
    assert!(ring.iter().filter(|g| **g != 0.0).count() > ring.len() / 4, "the ring's weights have no slope");
}

#[test]
fn the_search_can_lead_its_shots_and_expects_the_other_fighter_to() {
    use victris_engine::geom::chebyshev;
    use victris_engine::rules::AttackKind;
    use victris_engine::turn::{AttackIn, TargetIn};
    let e = Engine::shipped("columns-21");
    let net = random_net(31);
    let shot = e.rules.attacks.iter().position(|a| matches!(a.kind, AttackKind::Projectile { .. })).unwrap() as u8;
    let aim = |t: &RawTurn| match t.attack {
        AttackIn::Named { attack, target: TargetIn::Tile(p) } if attack == shot => Some(p),
        _ => None,
    };
    let (mut searched, mut with_leads, mut leads_tried, mut moved_on, mut stood) = (0, 0, 0, 0, 0);
    for seed in 0..10 {
        let game = random_game(&e, 200 + seed, 300);
        let mut s = e.new_match(game.seed);
        for (k, t) in game.ticks.iter().enumerate() {
            if k % 5 == 3 && s.result.is_none() {
                searched += 1;
                // without the switch every shot is aimed at the tile the other fighter stands on
                let mut plain = Search::new(&e, &net, PvConfig::default());
                plain.iterate(&s, 500);
                for x in 0..2 {
                    assert!(plain.root()[x].iter().all(|a| aim(&a.0).map(|p| p == s.fighters[1 - x].pos).unwrap_or(true)));
                }
                let cfg = PvConfig { lead_at: 100, ..Default::default() };
                let mut search = Search::new(&e, &net, cfg);
                search.iterate(&s, 500);
                let root = search.root();
                for x in 0..2 {
                    assert_eq!(root[x].iter().map(|a| a.1).sum::<u32>(), 500);
                    let at = s.fighters[1 - x].pos;
                    let leads: Vec<(RawTurn, u32)> = root[x].iter().filter(|a| aim(&a.0).map(|p| p != at).unwrap_or(false)).map(|a| (a.0, a.1)).collect();
                    if leads.is_empty() {
                        continue;
                    }
                    with_leads += 1;
                    for (i, a) in root[x].iter().enumerate() {
                        assert!(root[x][..i].iter().all(|b| b.0 != a.0), "a turn is in the list twice");
                    }
                    // the other fighter's turns here: can it move this tick?
                    let can_move = root[1 - x].iter().any(|a| a.0.mv != RawTurn::IDLE.mv || a.0.dash != RawTurn::IDLE.dash);
                    stood += !can_move as u32;
                    for (turn, n) in &leads {
                        let p = aim(turn).unwrap();
                        // aimed at a tile the other fighter can get to: a step off, or as far as a dash goes
                        assert!(chebyshev(p, at) <= if can_move { e.rules.dash_distance } else { 1 } && !e.map.is_wall(p), "aim {p:?} with the other fighter at {at:?}");
                        // a copy of a shot at the other fighter that is in the list
                        let direct = RawTurn { attack: AttackIn::Named { attack: shot, target: TargetIn::Tile(at) }, ..*turn };
                        assert!(root[x].iter().any(|b| b.0 == direct));
                        // and the engine fires it: the shot's cooldown starts
                        let mut pair = [RawTurn::IDLE; 2];
                        pair[x] = *turn;
                        let mut after = s;
                        e.step(&mut after, &pair);
                        assert!(after.fighters[x].cd_attack[shot as usize] > 0 || after.result.is_some(), "the engine did not fire {turn:?}");
                        leads_tried += (*n > 0) as u32;
                    }
                    // the tree moves on under a lead shot as under any turn
                    if let Some((lead, _)) = leads.iter().filter(|l| l.1 > 0).max_by_key(|l| l.1) {
                        let other = root[1 - x].iter().max_by_key(|a| a.1).unwrap().0;
                        let mut again = Search::new(&e, &net, cfg);
                        again.iterate(&s, 500);
                        let pair = if x == 0 { [*lead, other] } else { [other, *lead] };
                        let mut next = s;
                        e.step(&mut next, &pair);
                        if next.result.is_none() && again.advance(&pair[0], &pair[1]) {
                            moved_on += 1;
                            again.iterate(&next, 300);
                            assert_eq!(again.root()[0].iter().map(|a| a.1).sum::<u32>(), again.visits());
                        }
                    }
                }
                // on threads too
                let mut many = Search::new(&e, &net, cfg);
                assert_eq!(many.iterate_threads(&s, 500, 3, None), 500);
                assert_eq!(many.root()[0].iter().map(|a| a.1).sum::<u32>(), 500);
            }
            e.step(&mut s, &t.turns);
        }
    }
    println!("{searched} positions searched; lead shots in {with_leads} turn lists ({stood} with the other fighter unable to move this tick), {leads_tried} of them tried, the tree moved on under one {moved_on} times");
    assert!(with_leads >= 20 && leads_tried >= 50 && moved_on >= 3 && stood >= 3);
}

#[test]
fn a_full_tree_makes_room_and_goes_on_growing() {
    // the tree holds at most `max_nodes` positions; full, it drops the half the fewest lines
    // ran through and the search carries on (it used to take no new positions from then on)
    let e = Engine::shipped("columns-21");
    let net = random_net(41);
    let mut s = e.new_match(5);
    let mut rng = Rng::new(9);
    let mut buf = Vec::new();
    // a position in the middle of a random game
    for _ in 0..60 {
        let mut pair = [RawTurn::IDLE; 2];
        for x in 0..2 {
            turns(&e, &s, x, &mut buf);
            pair[x] = buf[rng.below(buf.len())];
        }
        e.step(&mut s, &pair);
    }
    assert!(s.result.is_none());
    let cap = 3000;
    let cfg = PvConfig { max_nodes: cap, ..Default::default() };
    let mut small = Search::new(&e, &net, cfg);
    let mut most = 0;
    let mut grew_after_full = false;
    let mut was_full = false;
    for _ in 0..40 {
        let before = small.nodes();
        small.iterate(&s, 500);
        most = most.max(small.nodes());
        was_full |= before + 500 > cap;
        grew_after_full |= was_full && small.nodes() != before;
        assert!(small.nodes() <= cap, "{} positions in a tree of at most {cap}", small.nodes());
    }
    assert!(was_full && grew_after_full, "the tree never filled, or stopped changing once full");
    assert!(most > cap / 2);
    // every line is still counted at the root, for both fighters
    assert_eq!(small.visits(), 20_000);
    for x in 0..2 {
        assert_eq!(small.root()[x].iter().map(|a| a.1).sum::<u32>(), 20_000);
    }
    // what is left is the searched part: every position in it is reachable, and the tree
    // moves on under the pair of turns both fighters like best
    let left = small.positions(&s);
    assert_eq!(left.len(), small.nodes(), "positions that cannot be reached from the root were kept");
    let pair = [small.best(0), small.best(1)];
    let mut next = s;
    e.step(&mut next, &pair);
    if next.result.is_none() {
        assert!(small.advance(&pair[0], &pair[1]), "the most searched pair of turns has no position under it");
        small.iterate(&next, 2000);
        assert!(small.nodes() <= cap);
    }
    // against a tree with all the room it wants, the same number of lines: the same first choice
    let mut big = Search::new(&e, &net, PvConfig::default());
    big.iterate(&s, 20_000);
    let mut again = Search::new(&e, &net, cfg);
    again.iterate(&s, 20_000);
    println!("{} positions with room for all, {} kept of at most {cap}; first choice {:?} / {:?}, value {:.3} / {:.3}", big.nodes(), again.nodes(), big.best(0), again.best(0), big.value(), again.value());
    assert!((big.value() - again.value()).abs() < 0.15);
    // on threads room is made before a run starts
    let mut many = Search::new(&e, &net, cfg);
    for _ in 0..12 {
        assert_eq!(many.iterate_threads(&s, 1000, 3, None), 1000);
        assert!(many.nodes() <= cap);
    }
    assert_eq!(many.root()[0].iter().map(|a| a.1).sum::<u32>(), 12_000);
    assert!(many.nodes() > cap / 2);
}

#[test]
fn positions_beside_the_path_are_saved_with_the_game_and_teach_their_value_alone() {
    let e = Engine::shipped("columns-21");
    // a game as self-play saves it with --side: at searched ticks, a pair of turns that was
    // not played and the search's value of the position after it
    let plain = random_game(&e, 31, 120);
    let mut game = plain.clone();
    let mut rng = Rng::new(77);
    let mut s = e.new_match(game.seed);
    let mut buf = Vec::new();
    let mut want: Vec<(victris_engine::State, f32)> = Vec::new();
    for (k, t) in game.ticks.iter_mut().enumerate() {
        if s.result.is_some() {
            break;
        }
        if t.pol.is_some() && k % 3 != 2 {
            for _ in 0..1 + k % 3 {
                let mut pair = [RawTurn::IDLE; 2];
                for x in 0..2 {
                    turns(&e, &s, x, &mut buf);
                    pair[x] = buf[rng.below(buf.len())];
                }
                let q = (rng.unit() as f32 - 0.5) * 1.6;
                t.side.push(SideLog { turns: pair, q, lines: 64 + rng.below(100_000) as u32 });
                let mut after = s;
                e.step(&mut after, &pair);
                if after.result.is_none() {
                    want.push((after, q));
                }
            }
        }
        e.step(&mut s, &t.turns);
    }
    assert!(want.len() > 60, "{} positions beside the path", want.len());
    // a file of games without such positions is the file older programs read; one with them
    // says so in its first bytes, and both kinds can follow each other in one file
    let mut bytes = Vec::new();
    plain.write(&mut bytes);
    assert_eq!(&bytes[..4], b"AZG2");
    let first = bytes.len();
    game.write(&mut bytes);
    assert_eq!(&bytes[first..first + 4], b"AZG3");
    plain.write(&mut bytes);
    let back = GameLog::read_all(&bytes);
    assert_eq!(back.len(), 3);
    assert!(back[0].ticks.iter().all(|t| t.side.is_empty()) && back[2].ticks.iter().all(|t| t.side.is_empty()));
    assert_eq!(back[2].ticks.len(), plain.ticks.len());
    for (x, y) in game.ticks.iter().zip(&back[1].ticks) {
        assert_eq!((x.turns, x.pol, x.side.len()), (y.turns, y.pol, y.side.len()));
        for (a, b) in x.side.iter().zip(&y.side) {
            assert_eq!(a.turns, b.turns);
            assert!((a.q - b.q).abs() < 0.005, "{} read back as {}", a.q, b.q);
            assert_eq!(b.lines, a.lines.min(65535));
        }
    }
    // a file cut off inside the positions beside the path loses that game only
    assert_eq!(GameLog::read_all(&bytes[..bytes.len() - plain_len(&plain) - 7]).len(), 1);
    // replayed: the game's own positions as before, and after each the positions beside it
    let mut own: Vec<Sample> = Vec::new();
    assert!(back[0].samples(&e, &mut own));
    let mut all: Vec<Sample> = Vec::new();
    assert!(back[1].samples(&e, &mut all));
    let on_path: Vec<&Sample> = all.iter().filter(|r| !r.side).collect();
    assert_eq!(on_path.len(), own.len());
    for (a, b) in on_path.iter().zip(&own) {
        assert_eq!((a.s, a.pol, a.z, a.has_q), (b.s, b.pol, b.z, b.has_q));
    }
    let beside: Vec<&Sample> = all.iter().filter(|r| r.side).collect();
    assert_eq!(beside.len(), want.len());
    let net = random_net(5);
    let n = net.layout().len;
    let mut sc = Scratch::default();
    for (r, (after, q)) in beside.iter().zip(&want) {
        assert_eq!(r.s, *after, "the position is the one the pair of turns leads to");
        assert!((r.q - q).abs() < 0.005);
        // its lesson is the search's value of it and nothing else: not the game's result
        // (the game did not go there), whatever the mix of the two is set to, and no turns
        for lambda in [0.0, 0.5, 1.0] {
            let t = victris_engine::pvnet::value_target(r, lambda);
            assert!((t - 0.5 * (1.0 + r.q)).abs() < 1e-6, "target {t} for a value of {}", r.q);
        }
        let mut grad = vec![0.0f32; n];
        let loss = loss_grad(&net, &e, r, 0.5, 1.0, &mut sc, Some(&mut grad));
        assert!(loss.side && !loss.decided);
        assert_eq!((loss.policy_n, loss.policy), (0, 0.0));
        assert!(loss.value > 0.0 && grad.iter().any(|g| *g != 0.0));
    }
    // a mirror image of such a position is still one
    let sym = Sym::of(&e);
    for k in (1..4).filter(|k| sym.ok[*k]) {
        let im = sym.image(k, beside[0]);
        assert!(im.side && im.q == beside[0].q);
    }
}

/// The bytes one game takes in a file.
fn plain_len(g: &GameLog) -> usize {
    let mut b = Vec::new();
    g.write(&mut b);
    b.len()
}

#[test]
fn looking_beside_the_path_leaves_the_game_as_it_was() {
    let e = Engine::shipped("columns-21");
    let net = random_net(8);
    let cfg = || PvConfig { even_root: 0.25, lead_at: 64, ..Default::default() };
    // the same game twice, side by side: one search only plays, the other also looks at
    // positions beside the path at every tick
    let mut plain = Search::new(&e, &net, cfg());
    let mut looks = Search::new(&e, &net, cfg());
    let mut rng = Rng::new(4);
    let mut rng_side = Rng::new(9);
    let mut s = e.new_match(6);
    let (mut seen, mut ticks) = (0, 0);
    for _ in 0..30 {
        if s.result.is_some() {
            break;
        }
        plain.iterate(&s, 160);
        looks.iterate(&s, 160);
        let (a, b) = (plain.root(), looks.root());
        assert_eq!(a, b, "tick {}: the searches differ", s.tick);
        assert_eq!(plain.value(), looks.value());
        let visits = |x: usize| a[x].iter().map(|t| (t.0, t.1)).collect::<Vec<_>>();
        let pair = [pick(&visits(0), 0.5, &mut rng), pick(&visits(1), 0.5, &mut rng)];
        let tried = looks.root_cells(0).cells.iter().filter(|c| c.1 > 0).count();
        let got = looks.beside(&s, &pair, 3, 40, &mut rng_side);
        assert!(got.len() <= 3);
        assert!(got.len() == 3 || tried < 4, "tick {}: {} of {tried} tried pairs", s.tick, got.len());
        for (k, (turns, q, lines)) in got.iter().enumerate() {
            assert!(*turns != pair, "the pair played is on the path, not beside it");
            assert!(got[..k].iter().all(|g| g.0 != *turns), "the same pair twice");
            assert!(*lines >= 40, "{lines} lines behind a position beside the path");
            assert!(q.abs() <= 1.0);
            // the pair is one both fighters could play, and the game is not over after it
            assert!(a[0].iter().any(|t| t.0 == turns[0]) && a[1].iter().any(|t| t.0 == turns[1]));
            let mut after = s;
            e.step(&mut after, turns);
            assert!(after.result.is_none());
            seen += 1;
        }
        e.step(&mut s, &pair);
        assert_eq!(plain.advance(&pair[0], &pair[1]), looks.advance(&pair[0], &pair[1]));
        assert_eq!(plain.nodes(), looks.nodes(), "tick {}: what was kept differs", s.tick);
        ticks += 1;
    }
    assert!(ticks >= 20 && seen >= 40, "{seen} positions beside the path in {ticks} ticks");
}
