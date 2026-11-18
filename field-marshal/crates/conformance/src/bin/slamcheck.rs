//! slamcheck: why does the search walk into wall slams? Tick by tick.
//!
//!   slamcheck <replay dir> --bot A [--iterations 27000] [--show 8] [--dash-burst]
//!
//! For every wall slam the named bot takes in the replays, looks at the two ticks before it:
//! where both stood, the slammer's repulsor and dash cooldowns, the turn the bot played,
//! and what a fresh search (generation 1 judgment, the given iterations) makes of that
//! position: its preferred turn and value, the value of the turn played, and how likely the
//! search thought the opponent's repulsor was (its prior and its share of the opponent's
//! visits). Ends with counts: slams the search saw coming and could have avoided, slams it
//! saw but rated as unavoidable, and slams it did not see.

use flate2::read::GzDecoder;
use std::io::Read;
use victris_engine::events::Event;
use victris_engine::geom::{chebyshev, DIR_NAMES};
use victris_engine::mcts::{actions_with, Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::State;
use victris_engine::turn::{AttackIn, DirIn, RawTurn};
use victris_engine::Engine;

fn describe(e: &Engine, t: &RawTurn) -> String {
    let mut out = Vec::new();
    if let DirIn::Dir(d) = t.mv {
        out.push(format!("step {}", DIR_NAMES[d as usize]));
    }
    if let DirIn::Dir(d) = t.dash {
        out.push(format!("dash {}", DIR_NAMES[d as usize]));
    }
    if let AttackIn::Named { attack, .. } = t.attack {
        out.push(e.rules.attacks[attack as usize].name.clone());
    }
    if out.is_empty() {
        "hold".into()
    } else {
        out.join(" + ")
    }
}

fn is_burst(e: &Engine, t: &RawTurn) -> bool {
    matches!(t.attack, AttackIn::Named { attack, .. } if matches!(e.rules.attacks[attack as usize].kind, AttackKind::Burst { .. }))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").unwrap_or_else(|| "A".into());
    let iterations: u32 = opt("--iterations").and_then(|s| s.parse().ok()).unwrap_or(27_000);
    let dash_burst = args.iter().any(|a| a == "--dash-burst");
    let show: usize = opt("--show").and_then(|s| s.parse().ok()).unwrap_or(8);
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let (mut slams, mut damage, mut games) = (0usize, 0i32, 0usize);
    let (mut avoidable, mut seen_unavoidable, mut unseen) = (0usize, 0usize, 0usize);
    let mut shown = 0usize;
    for path in files {
        let name = path.to_string_lossy().to_string();
        let mut text = String::new();
        if name.ends_with(".gz") {
            if std::fs::File::open(&path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                continue;
            }
        } else if name.ends_with(".jsonl") {
            text = std::fs::read_to_string(&path).unwrap_or_default();
        } else {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        games += 1;
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut history: Vec<(State, [RawTurn; 2])> = Vec::new();
        let mut events = Vec::new();
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let before = s;
            e.step_events(&mut s, &t.turns, &mut events);
            history.push((before, t.turns));
            for ev in &events {
                let Event::WallSlam { target, damage: d, .. } = ev else { continue };
                if *target as usize != me + 1 {
                    continue;
                }
                slams += 1;
                damage += d;
                // The tick the repulsor went off, and the one before.
                let n = history.len();
                let mut verdict = 0; // 0 unseen, 1 seen but unavoidable, 2 avoidable
                let mut lines = Vec::new();
                for back in (0..2usize).rev() {
                    if n < back + 1 {
                        continue;
                    }
                    let (st, turns) = history[n - 1 - back];
                    let (f, o) = (&st.fighters[me], &st.fighters[1 - me]);
                    let burst_cd = e.rules.attacks.iter().enumerate().filter(|(_, a)| matches!(a.kind, AttackKind::Burst { .. })).map(|(i, _)| o.cd_attack[i]).min().unwrap_or(99);
                    let cfg = Config { value_weight: 1.0, value_gen: 1, buff_value: 0.0, dash_burst, ..Default::default() };
                    let mut search = Search::new(e, cfg);
                    search.start();
                    search.iterate(&st, [None, None], iterations);
                    let root = search.result();
                    let mine = &root.acts[me];
                    let theirs = &root.acts[1 - me];
                    let best = mine.iter().max_by_key(|a| a.1).copied();
                    let played = mine.iter().find(|a| a.0 == turns[me]).copied();
                    let total_theirs: u32 = theirs.iter().map(|a| a.1).sum();
                    let burst_visits: u32 = theirs.iter().filter(|a| is_burst(e, &a.0)).map(|a| a.1).sum();
                    let prior_burst: f32 = actions_with(e, &st, 1 - me, cfg.even, false, false, false, dash_burst).iter().filter(|a| is_burst(e, &a.turn)).map(|a| a.prior).sum();
                    let share = burst_visits as f64 / total_theirs.max(1) as f64;
                    let (bq, pq) = (best.map(|b| b.2).unwrap_or(0.0), played.map(|p| p.2).unwrap_or(f64::NAN));
                    if back == 1 || n < 2 {
                        verdict = if share < 0.1 { 0 } else if bq - pq > 4.0 { 2 } else { 1 };
                    }
                    lines.push(format!(
                        "    tick {:>3}: us {:?} hp {} cd step {} dash {} | them {:?} repulsor cd {} dash cd {} | {} apart | we played {} (value {:+.1}); search prefers {} ({:+.1}); their repulsor: prior {:.0}%, visits {:.0}%; they played {}",
                        st.tick, f.pos, f.hp, f.cd_move, f.cd_dash, o.pos, burst_cd, o.cd_dash, chebyshev(f.pos, o.pos),
                        describe(e, &turns[me]), pq, best.map(|b| describe(e, &b.0)).unwrap_or_default(), bq, 100.0 * prior_burst, 100.0 * share, describe(e, &turns[1 - me])
                    ));
                }
                match verdict {
                    0 => unseen += 1,
                    1 => seen_unavoidable += 1,
                    _ => avoidable += 1,
                }
                if shown < show {
                    shown += 1;
                    println!("{} {} tick {}: slammed for {} ({})", e.map.name, path.file_name().unwrap().to_string_lossy(), s.tick, d, ["not foreseen", "foreseen, rated unavoidable", "foreseen and avoidable"][verdict]);
                    for l in &lines {
                        println!("{l}");
                    }
                }
            }
        }
    }
    println!("\n{games} games of {bot}: {slams} wall slams taken, {damage} hp ({:.1} a game)", damage as f64 / games.max(1) as f64);
    println!("  the search, one tick before the repulsor went off: did not foresee it {unseen}, foresaw it but rated it unavoidable {seen_unavoidable}, foresaw it and had a turn worth 4 hp more {avoidable}");
}
