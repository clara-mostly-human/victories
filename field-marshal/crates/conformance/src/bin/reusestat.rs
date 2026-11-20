//! reusestat: how much of each search arrives already built through tree reuse.
//!
//!   reusestat <replay dirs> --bot field-marshal [--games 20] [--ms 150]
//!
//! Replays the named bot's games tick by tick. At every tick where it can act it searches for
//! `--ms` the way the flagship does (row 13, dash bursts, tree reuse): if the previous tick's
//! tree has a branch for (our turn actually played, the opponent's turn actually played), the
//! search starts from it. Reports how often that branch exists, and the visits it carries as a
//! share of the new search's total.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;
use victris_engine::mcts::{Config, Search};
use victris_engine::replay::parse_replay;
use victris_engine::state::FState;

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").unwrap_or_else(|| "field-marshal".into());
    let games: usize = opt("--games").and_then(|s| s.parse().ok()).unwrap_or(20);
    let ms: f64 = opt("--ms").and_then(|s| s.parse().ok()).unwrap_or(150.0);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bot" || a == "--games" || a == "--ms" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let cfg = Config { value_weight: 1.0, value_gen: 13, buff_value: 0.0, dash_burst: true, own_priors: args.iter().any(|a| a == "--own-priors"), ..Default::default() };
    // how well each search foresaw the reply: the share of the opponent's root visits on the turn
    // it then actually played, and that turn's rank among its root turns
    let (mut reply_share_sum, mut reply_n, mut reply_top, mut reply_top3) = (0.0f64, 0usize, 0usize, 0usize);
    let (mut decisions, mut hits, mut miss_gap) = (0usize, 0usize, 0usize);
    let mut shares: Vec<f64> = Vec::new();
    let (mut carried_sum, mut total_sum) = (0u64, 0u64);
    let mut played = 0usize;
    for path in &files {
        if played >= games {
            break;
        }
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        played += 1;
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        // the previous tick's search, its root, the tick, and the turn we actually played
        let mut kept: Option<(Search, victris_engine::state::State, i32, victris_engine::turn::RawTurn)> = None;
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let f = &s.fighters[me];
            if f.state == FState::Idle && f.alive && s.fighters[1 - me].alive {
                decisions += 1;
                let mut search = Search::new(e, cfg);
                let mut carried = 0u64;
                match kept.take() {
                    Some((old, old_root, tick, ours)) if tick + 1 == s.tick => match old.reroot(&old_root, me, &ours, &s) {
                        Some(tree) => {
                            carried = tree.visits() as u64;
                            hits += 1;
                            search.start_from(tree);
                        }
                        None => search.start(),
                    },
                    Some(_) => {
                        miss_gap += 1;
                        search.start();
                    }
                    None => search.start(),
                }
                let t0 = Instant::now();
                let mut fresh = 0u64;
                while t0.elapsed().as_secs_f64() * 1000.0 < ms {
                    search.iterate(&s, [None, None], 100);
                    fresh += 100;
                }
                {
                    let res = search.result();
                    let theirs = &res.acts[1 - me];
                    let tot: u32 = theirs.iter().map(|a| a.1).sum();
                    if tot > 0 {
                        let actual = t.turns[1 - me];
                        let mut sorted: Vec<&(victris_engine::turn::RawTurn, u32, f64)> = theirs.iter().collect();
                        sorted.sort_by(|a, b| b.1.cmp(&a.1));
                        if let Some(rank) = sorted.iter().position(|a| a.0 == actual) {
                            reply_share_sum += sorted[rank].1 as f64 / tot as f64;
                            reply_top += (rank == 0) as usize;
                            reply_top3 += (rank < 3) as usize;
                        }
                        reply_n += 1;
                    }
                }
                let total = carried + fresh;
                carried_sum += carried;
                total_sum += total;
                shares.push(carried as f64 / total.max(1) as f64);
                kept = Some((search, s, s.tick, t.turns[me]));
            } else {
                kept = None;
            }
            e.step(&mut s, &t.turns);
        }
    }
    shares.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let q = |p: f64| shares.get(((shares.len() as f64 - 1.0) * p) as usize).cloned().unwrap_or(0.0);
    println!("{played} games of {bot}, {decisions} decisions searched for {ms} ms each");
    println!("  previous tick's tree had the branch actually played: {:.0}% of decisions ({:.0}% had no tree from the tick before: we could not act then)", 100.0 * hits as f64 / decisions.max(1) as f64, 100.0 * miss_gap as f64 / decisions.max(1) as f64);
    let rn = reply_n.max(1) as f64;
    println!("  the opponent's actual reply: {:.0}% of its root visits on average; its most visited turn {:.0}% of the time, in its top three {:.0}%", 100.0 * reply_share_sum / rn, 100.0 * reply_top as f64 / rn, 100.0 * reply_top3 as f64 / rn);
    println!("  visits arriving pre-built, as a share of each search's total: mean {:.0}%, median {:.0}%, top quarter above {:.0}%, top tenth above {:.0}%", 100.0 * carried_sum as f64 / total_sum.max(1) as f64, 100.0 * q(0.5), 100.0 * q(0.75), 100.0 * q(0.9));
}
