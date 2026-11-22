//! refight: what the bot's search saw in a game it played on the ladder.
//!
//!   refight FIGHT.jsonl PLAYBOOK.json --ticks 22,59,87 [--iters 35000] [--pv DIR] [--top 8]
//!
//! FIGHT.jsonl is one game as the gateway saved it (data/fights/<day>/<match>-<bot>.jsonl.gz,
//! gunzipped): every message in and every turn out. The game is fed back through the tracker
//! tick by tick with the turns that were really sent, so the bot's picture of the game is the
//! one it had. At each tick asked for, the decision is made again with PLAYBOOK.json (the
//! fields of `Playbook`, or a file with a "default" object) and both fighters' turns at the
//! root of the search are printed: visits and mean result, the most visited first, and what
//! share of the opponent's visits went to a dash with a repulsor.
//!
//! --table N goes further: every one of our turns against every one of theirs, each valued by
//! a search of N lines, and the table solved as a game (regret matching): what share each turn
//! gets when neither side can be exploited, beside the share the search gave it.
//!
//! --weigh SHARE turns on `tree_pv_weigh` (the turn chosen by `Search::weigh`, SHARE of the
//! lines held back for it) and prints what each of our turns came to; --trust T sets
//! `tree_pv_trust` (1 unless given).
//!
//! --iters fixes the lines searched (a cloud core is slower than the Mac, so the live
//! thinking time would search fewer). --pv points `tree_pv_dir` at another folder of networks.
//! A tick is searched fresh unless the tick before it was asked for too: ask for a run of
//! ticks (57,58,59) to carry the tree forward as the live bot did.
//!
//! --cross N adds a table for each tick: our most visited turns (and the one sent) against
//! their most visited turns and their dash-with-repulsor turns. Each cell plays that pair of
//! turns on the engine and gives the position after it: hit points, who is stunned, the
//! network's value for us at once, and the value of a fresh search of N lines from there.

use marshal::policy::{decide, Playbook, LAST_ROOT};
use marshal::session::turn_json;
use marshal::tracker::Tracker;
use serde_json::Value;
use victris_engine::observe::View;
use victris_engine::rules::AttackKind;
use victris_engine::turn::{AttackIn, DirIn};
use victris_engine::RawTurn;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let text = std::fs::read_to_string(&args[0]).expect("cannot read the game");
    let mut pbv: Value = serde_json::from_str(&std::fs::read_to_string(&args[1]).expect("cannot read the playbook")).expect("the playbook is not JSON");
    if let Some(d) = pbv.get("default").cloned() {
        pbv = d;
    }
    if let Some(n) = opt("--iters").and_then(|s| s.parse::<u64>().ok()) {
        pbv["tree_iters"] = Value::from(n);
    }
    if let Some(dir) = opt("--pv") {
        pbv["tree_pv_dir"] = Value::from(dir);
    }
    if let Some(w) = opt("--weigh").and_then(|s| s.parse::<f64>().ok()) {
        pbv["tree_pv_weigh"] = Value::from(w);
    }
    if let Some(w) = opt("--trust").and_then(|s| s.parse::<f64>().ok()) {
        pbv["tree_pv_trust"] = Value::from(w);
    }
    let pb = Playbook::from_json(pbv.get("name").and_then(Value::as_str).unwrap_or("refight"), &pbv);
    let ticks: Vec<i64> = opt("--ticks").unwrap_or_default().split(',').filter_map(|s| s.trim().parse().ok()).collect();
    let top: usize = opt("--top").and_then(|s| s.parse().ok()).unwrap_or(8);
    let cross: u32 = opt("--cross").and_then(|s| s.parse().ok()).unwrap_or(0);
    let table: u32 = opt("--table").and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut net: Option<victris_engine::pvnet::PvNet> = None;

    let mut tracker: Option<Tracker> = None;
    let mut sent: std::collections::HashMap<i64, Value> = std::collections::HashMap::new();
    let lines: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    for l in &lines {
        if l.get("t").and_then(Value::as_str) == Some("out") {
            if let Some(t) = l.get("msg").and_then(|m| m.get("tick")).and_then(Value::as_i64) {
                sent.insert(t, l["msg"].clone());
            }
        }
    }
    for l in &lines {
        if l.get("t").and_then(Value::as_str) != Some("in") {
            continue;
        }
        let msg = &l["msg"];
        match msg.get("type").and_then(Value::as_str) {
            Some("hello") => {
                tracker = Some(Tracker::new(msg).expect("the hello message was not understood"));
                println!("{} against {} on {}, playbook {}", msg["match_id"], msg["opponent"]["name"], msg["map_name"], pb.name);
            }
            Some("observation") => {
                let Some(t) = tracker.as_mut() else { continue };
                let tick = msg["tick"].as_i64().unwrap_or(0);
                let view = match View::from_json(&t.engine, msg) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("tick {tick}: observation not understood: {e}");
                        continue;
                    }
                };
                let events: Vec<Value> = msg.get("events").and_then(Value::as_array).cloned().unwrap_or_default();
                let prev = sent.get(&(tick - 1)).map(|m| RawTurn::from_json(Some(m), &t.engine.rules));
                let sync = t.observe(&view, &events, prev);
                if !ticks.contains(&tick) {
                    continue;
                }
                LAST_ROOT.with(|r| *r.borrow_mut() = None);
                let d = decide(t, &view, &pb);
                let me = t.me;
                let show = |turn: &RawTurn| {
                    let mut v = turn_json(tick as i32, turn, &t.engine);
                    if let Some(o) = v.as_object_mut() {
                        o.remove("tick");
                    }
                    if v.as_object().map(|o| o.is_empty()).unwrap_or(true) { "stay".to_string() } else { v.to_string() }
                };
                let was = sent.get(&tick).map(|m| RawTurn::from_json(Some(m), &t.engine.rules)).unwrap_or(RawTurn::IDLE);
                println!("\ntick {tick} ({}): sent in the game {} | decided now {} ({}, {} lines, score {:.2})", sync.name(), show(&was), show(&d.turn), d.note, d.sims, d.score);
                let seen = LAST_ROOT.with(|r| r.borrow_mut().take());
                let Some(root) = seen else {
                    println!("  no search at this tick");
                    continue;
                };
                let f = &root.root.fighters;
                println!(
                    "  the search's position: us {:?} hp {} | them {:?} hp {} dash cd {} power {} | its value for us {:+.2}",
                    f[me].pos,
                    f[me].hp,
                    f[1 - me].pos,
                    f[1 - me].hp,
                    f[1 - me].cd_dash,
                    if f[1 - me].power_until > root.root.tick { f[1 - me].power_until - root.root.tick } else { 0 },
                    if me == 0 { root.value } else { -root.value }
                );
                if !root.weighed.is_empty() {
                    println!("  our turns weighed, the turn to play first (worth | against the turns expected of them | against their best answer to our usual play | lines):");
                    for w in root.weighed.iter() {
                        // at trust 1 no answer of theirs is looked for
                        let answer = if w.worst_to == RawTurn::IDLE && w.worst == w.expected { String::from("(not looked for)") } else { format!("{:+.3} to {}", w.worst, show(&w.worst_to)) };
                        println!("    {:+.3} | {:+.3} | {} | {:>6} lines  <- {}", w.worth, w.expected, answer, w.lines, show(&w.turn));
                    }
                }
                let is_slam = |turn: &RawTurn| turn.dash != DirIn::None && matches!(turn.attack, AttackIn::Named { attack, .. } if matches!(t.engine.rules.attacks[attack as usize].kind, AttackKind::Burst { .. }));
                if net.is_none() && !pb.tree_pv_dir.is_empty() {
                    net = victris_engine::pvnet::PvNet::load(&std::path::Path::new(&pb.tree_pv_dir).join(format!("{}.json", t.engine.map.name)));
                }
                // the network's share for a turn of fighter x at the root, before any search
                let ev = net.as_ref().map(|n| n.eval(&t.engine, &root.root));
                let share = |x: usize, turn: &RawTurn| -> f64 {
                    let Some(ev) = ev.as_ref() else { return 0.0 };
                    let l = &ev.logits[x];
                    let top = root.acts[x].iter().map(|a| l[victris_engine::pvnet::class_of(&a.0)]).fold(f32::MIN, f32::max);
                    let total: f64 = root.acts[x].iter().map(|a| ((l[victris_engine::pvnet::class_of(&a.0)] - top) as f64).exp()).sum();
                    ((l[victris_engine::pvnet::class_of(turn)] - top) as f64).exp() / total.max(1e-12)
                };
                for x in [me, 1 - me] {
                    let mut acts = root.acts[x].clone();
                    acts.sort_by(|a, b| b.1.cmp(&a.1));
                    let total: u32 = acts.iter().map(|a| a.1).sum::<u32>().max(1);
                    let slam: u32 = acts.iter().filter(|a| is_slam(&a.0)).map(|a| a.1).sum();
                    let slam_share: f64 = acts.iter().filter(|a| is_slam(&a.0)).map(|a| share(x, &a.0)).sum();
                    println!(
                        "  {} ({} turns; dash with repulsor: {:.1}% of visits, the network alone gave it {:.1}%):",
                        if x == me { "our turns" } else { "their turns as the search expects them" },
                        acts.len(),
                        100.0 * slam as f64 / total as f64,
                        100.0 * slam_share
                    );
                    for a in acts.iter().take(top) {
                        println!("    {:>5.1}%  (network {:>4.1}%)  result {:+.2}  {}", 100.0 * a.1 as f64 / total as f64, 100.0 * share(x, &a.0), a.2, show(&a.0));
                    }
                    if x != me {
                        let mut s: Vec<_> = acts.iter().filter(|a| is_slam(&a.0)).collect();
                        s.sort_by(|a, b| b.1.cmp(&a.1));
                        for a in s.iter().take(4) {
                            println!("    dash with repulsor: {:>5.2}%  (network {:>4.1}%)  result {:+.2}  {}", 100.0 * a.1 as f64 / total as f64, 100.0 * share(x, &a.0), a.2, show(&a.0));
                        }
                    }
                }
                if table > 0 {
                    // Every one of our turns against every one of theirs: the position after the
                    // pair, valued by a fresh search of `table` lines. Then the game of choosing a
                    // turn without knowing the other's is solved on that table.
                    let Some(net) = net.as_ref() else { continue };
                    let side = if me == 0 { 1.0f64 } else { -1.0 };
                    let (ours, theirs) = (&root.acts[me], &root.acts[1 - me]);
                    let mut m = vec![vec![0.0f64; theirs.len()]; ours.len()];
                    for (i, a) in ours.iter().enumerate() {
                        for (j, b) in theirs.iter().enumerate() {
                            let mut c = root.root;
                            let pair = if me == 0 { [a.0, b.0] } else { [b.0, a.0] };
                            t.engine.step(&mut c, &pair);
                            m[i][j] = match victris_engine::pvsearch::outcome(&c) {
                                Some(z) => z as f64 * side,
                                None => {
                                    let mut search = victris_engine::pvsearch::Search::new(&t.engine, net, victris_engine::pvsearch::PvConfig::default());
                                    search.iterate(&c, table);
                                    search.value() as f64 * side
                                }
                            };
                        }
                    }
                    // regret matching, both sides, averaged: converges to the solution of the table
                    let (na, nb) = (ours.len(), theirs.len());
                    let (mut ra, mut rb) = (vec![0.0f64; na], vec![0.0f64; nb]);
                    let (mut sa, mut sb) = (vec![0.0f64; na], vec![0.0f64; nb]);
                    let strat = |r: &Vec<f64>| {
                        let tot: f64 = r.iter().map(|v| v.max(0.0)).sum();
                        r.iter().map(|v| if tot > 0.0 { v.max(0.0) / tot } else { 1.0 / r.len() as f64 }).collect::<Vec<f64>>()
                    };
                    for it in 0..200_000u32 {
                        let (pa, pb_) = (strat(&ra), strat(&rb));
                        let ua: Vec<f64> = (0..na).map(|i| (0..nb).map(|j| pb_[j] * m[i][j]).sum()).collect();
                        let ub: Vec<f64> = (0..nb).map(|j| (0..na).map(|i| -pa[i] * m[i][j]).sum()).collect();
                        let va: f64 = (0..na).map(|i| pa[i] * ua[i]).sum();
                        for i in 0..na {
                            ra[i] = (ra[i] + ua[i] - va).max(0.0);
                            sa[i] += pa[i] * it as f64;
                        }
                        for j in 0..nb {
                            rb[j] = (rb[j] + ub[j] + va).max(0.0);
                            sb[j] += pb_[j] * it as f64;
                        }
                    }
                    let norm = |v: &Vec<f64>| {
                        let t: f64 = v.iter().sum();
                        v.iter().map(|x| x / t).collect::<Vec<f64>>()
                    };
                    let (pa, pb_) = (norm(&sa), norm(&sb));
                    let against = |i: usize, mix: &Vec<f64>| (0..nb).map(|j| mix[j] * m[i][j]).sum::<f64>();
                    let worst = |i: usize| (0..nb).map(|j| (m[i][j], j)).fold((f64::MAX, 0), |a, b| if b.0 < a.0 { b } else { a });
                    let value: f64 = (0..na).map(|i| pa[i] * against(i, &pb_)).sum();
                    let tot_a: f64 = ours.iter().map(|a| a.1 as f64).sum::<f64>().max(1.0);
                    let tot_b: f64 = theirs.iter().map(|a| a.1 as f64).sum::<f64>().max(1.0);
                    let seen: Vec<f64> = theirs.iter().map(|a| a.1 as f64 / tot_b).collect();
                    println!("  the table solved ({} x {} turns, {} lines a cell): worth {:+.3} to us", na, nb, table, value);
                    println!("  our turns: share when solved | share of the search's visits | against their solved mix | against the mix the search expected | worst case");
                    let mut order: Vec<usize> = (0..na).collect();
                    order.sort_by(|x, y| (pa[*y] + ours[*y].1 as f64 / tot_a).partial_cmp(&(pa[*x] + ours[*x].1 as f64 / tot_a)).unwrap());
                    for &i in order.iter().take(top.max(8)) {
                        let (w, wj) = worst(i);
                        println!(
                            "    {:>5.1}% | {:>5.1}% | {:+.3} | {:+.3} | {:+.2} to {}  <- {}",
                            100.0 * pa[i],
                            100.0 * ours[i].1 as f64 / tot_a,
                            against(i, &pb_),
                            against(i, &seen),
                            w,
                            show(&theirs[wj].0),
                            show(&ours[i].0)
                        );
                    }
                    println!("  their turns: share when solved | share of the search's visits | network's share");
                    let mut order: Vec<usize> = (0..nb).collect();
                    order.sort_by(|x, y| (pb_[*y] + seen[*y]).partial_cmp(&(pb_[*x] + seen[*x])).unwrap());
                    for &j in order.iter().take(top.max(8)) {
                        println!("    {:>5.1}% | {:>5.1}% | {:>5.1}%  <- {}", 100.0 * pb_[j], 100.0 * seen[j], 100.0 * share(1 - me, &theirs[j].0), show(&theirs[j].0));
                    }
                    let slam_solved: f64 = (0..nb).filter(|j| is_slam(&theirs[*j].0)).map(|j| pb_[j]).sum();
                    println!("  their dash with repulsor when solved: {:.1}%", 100.0 * slam_solved);
                }
                if cross > 0 {
                    let Some(net) = net.as_ref() else {
                        println!("  no network for this map in {}", pb.tree_pv_dir);
                        continue;
                    };
                    let most = |x: usize, n: usize| {
                        let mut a = root.acts[x].clone();
                        a.sort_by(|a, b| b.1.cmp(&a.1));
                        a.into_iter().take(n).map(|a| a.0).collect::<Vec<RawTurn>>()
                    };
                    let mut ours = most(me, 4);
                    if !ours.contains(&was) {
                        ours.push(was);
                    }
                    let mut theirs = most(1 - me, 3);
                    let mut slams: Vec<_> = root.acts[1 - me].iter().filter(|a| is_slam(&a.0)).cloned().collect();
                    slams.sort_by(|a, b| b.1.cmp(&a.1));
                    for a in slams.iter().take(3) {
                        if !theirs.contains(&a.0) {
                            theirs.push(a.0);
                        }
                    }
                    println!("  after one tick (our hp/their hp, s = stunned; the network's value for us; a search of {cross} lines from there):");
                    for b in &theirs {
                        println!("    they play {}", show(b));
                        for a in &ours {
                            let mut c = root.root;
                            let pair = if me == 0 { [*a, *b] } else { [*b, *a] };
                            t.engine.step(&mut c, &pair);
                            let side = if me == 0 { 1.0 } else { -1.0 };
                            let (nv, sv) = match victris_engine::pvsearch::outcome(&c) {
                                Some(z) => (z * side, z * side),
                                None => {
                                    let mut search = victris_engine::pvsearch::Search::new(&t.engine, net, victris_engine::pvsearch::PvConfig::default());
                                    search.iterate(&c, cross);
                                    (search.net_value() * side, search.value() * side)
                                }
                            };
                            let st = |x: usize| if c.fighters[x].state == victris_engine::state::FState::Stunned { "s" } else { " " };
                            println!(
                                "      {:>3}{}/{:>3}{}  network {:+.2}  search {:+.2}  we play {}",
                                c.fighters[me].hp,
                                st(me),
                                c.fighters[1 - me].hp,
                                st(1 - me),
                                nv,
                                sv,
                                show(a)
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
}
