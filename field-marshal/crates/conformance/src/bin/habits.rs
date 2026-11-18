//! habits: put our policy in a strong bot's shoes and compare what it would do with what
//! the bot did, tick by tick, over ladder replays.
//!
//!   habits --bot StuartSutcliffe [--playbook file.json] [--limit 60] [--threads 4] <replay dir or files>...
//!
//! For every tick where the named bot could act and had the opponent in sight, the
//! replay's true state is handed to our policy as if it were ours. The tool tallies how
//! often we agree on holding, stepping, dashing and each attack, by range and by whose
//! shot is ready. It is a yardstick that does not depend on self-play: thousands of
//! decisions by the ladder's best, in the positions they actually reached.

use flate2::read::GzDecoder;
use marshal::policy::{decide, Playbook};
use marshal::tracker::Tracker;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::chebyshev;
use victris_engine::observe::{can_see, View};
use victris_engine::replay::parse_replay;
use victris_engine::rules::AttackKind;
use victris_engine::state::FState;
use victris_engine::turn::{AttackIn, DirIn, RawTurn};

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else if path.to_string_lossy().ends_with(".jsonl.gz") {
        out.push(path.to_path_buf());
    }
}

fn movement(t: &RawTurn) -> &'static str {
    match (t.mv, t.dash) {
        (_, DirIn::Dir(_)) => "dash",
        (DirIn::Dir(_), _) => "step",
        _ => "hold",
    }
}

fn attack(t: &RawTurn, e: &victris_engine::Engine) -> &'static str {
    match t.attack {
        AttackIn::Named { attack, .. } => match e.rules.attacks[attack as usize].kind {
            AttackKind::Projectile { .. } => "shot",
            AttackKind::Area { .. } => "frost",
            AttackKind::Burst { .. } => "repulsor",
        },
        _ => "none",
    }
}

type Tally = BTreeMap<String, [u32; 2]>; // key -> [count, agreements or events]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").expect("--bot NAME");
    let limit: usize = opt("--limit").and_then(|s| s.parse().ok()).unwrap_or(60);
    let threads: usize = opt("--threads").and_then(|s| s.parse().ok()).unwrap_or(4);
    let pb = match opt("--playbook") {
        None => Playbook::default(),
        Some(p) => Playbook::from_json("candidate", &serde_json::from_str(&std::fs::read_to_string(&p).expect("playbook file")).expect("playbook json")),
    };
    let mut files = Vec::new();
    let flags = ["--bot", "--limit", "--threads", "--playbook"];
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if flags.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        collect(Path::new(a), &mut files);
    }
    files.reverse(); // newest first
    let next = std::sync::atomic::AtomicUsize::new(0);
    let used = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<(Tally, u32)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads.max(1))
            .map(|_| {
                scope.spawn(|| {
                    let mut tally = Tally::new();
                    let mut ticks = 0u32;
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(path) = files.get(i) else { break };
                        if used.load(std::sync::atomic::Ordering::Relaxed) >= limit {
                            break;
                        }
                        let mut text = String::new();
                        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
                            continue;
                        }
                        let Ok(replay) = parse_replay(&text) else { continue };
                        let Some(x) = replay.bots.iter().position(|b| *b == bot) else { continue };
                        if x > 1 {
                            continue;
                        }
                        used.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let e = &replay.engine;
                        let mut tracker = Tracker::for_engine(e.clone(), x, replay.seed);
                        let mut s = e.new_match(replay.seed);
                        for t in &replay.ticks {
                            if s.result.is_some() {
                                break;
                            }
                            let (f, o) = (s.fighters[x], s.fighters[1 - x]);
                            if f.state == FState::Idle && can_see(&e.rules, &e.map, f.pos, o.pos) {
                                tracker.belief = Some(s);
                                tracker.enemy_visible = true;
                                let view = View::from_state(e, &s, x);
                                let ours = decide(&tracker, &view, &pb).turn;
                                let theirs = t.turns[x];
                                ticks += 1;
                                let d = chebyshev(f.pos, o.pos);
                                let band = if d <= 2 { "<=2" } else if d <= 4 { "3-4" } else { "5+" };
                                let ready = |who: &victris_engine::Fighter| {
                                    e.rules.attacks.iter().enumerate().any(|(i, a)| matches!(a.kind, AttackKind::Projectile { .. }) && who.cd_attack[i] <= 1)
                                };
                                let ctx = format!(
                                    "{band} | their shot {} | own shot {} | step {}",
                                    if ready(&o) { "ready" } else { "away " },
                                    if ready(&f) { "ready" } else { "away " },
                                    if f.cd_move == 0 { "ready" } else { "spent" }
                                );
                                let mut bump = |k: String, hit: bool| {
                                    let c = tally.entry(k).or_insert([0, 0]);
                                    c[0] += 1;
                                    c[1] += hit as u32;
                                };
                                let (tm, om) = (movement(&theirs), movement(&ours));
                                let (ta, oa) = (attack(&theirs, e), attack(&ours, e));
                                bump("A  movement agrees".into(), tm == om);
                                bump("A  attack agrees".into(), ta == oa);
                                bump(format!("B  they {tm:<5} we {om}"), false);
                                bump(format!("C  they {ta:<8} we {oa}"), false);
                                if f.cd_move == 0 {
                                    bump(format!("D  {ctx} : they stepped"), tm == "step");
                                    bump(format!("E  {ctx} : we stepped"), om == "step");
                                }
                            }
                            e.step(&mut s, &t.turns);
                        }
                    }
                    (tally, ticks)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut total = Tally::new();
    let mut ticks = 0;
    for (t, n) in results {
        ticks += n;
        for (k, v) in t {
            let c = total.entry(k).or_insert([0, 0]);
            c[0] += v[0];
            c[1] += v[1];
        }
    }
    println!("{bot}: {} games, {ticks} decisions with the opponent in sight, playbook {}", used.load(std::sync::atomic::Ordering::Relaxed), pb.name);
    for (k, v) in &total {
        match &k[..1] {
            "A" => println!("  {:<24} {:>5.1}%", &k[3..], 100.0 * v[1] as f64 / v[0] as f64),
            "B" | "C" => println!("  {:<28} {:>6}  ({:.1}%)", &k[3..], v[0], 100.0 * v[0] as f64 / ticks as f64),
            _ => {
                if v[0] >= 60 {
                    println!("  {:<62} {:>5.0}%  of {}", &k[3..], 100.0 * v[1] as f64 / v[0] as f64, v[0])
                }
            }
        }
    }
}
