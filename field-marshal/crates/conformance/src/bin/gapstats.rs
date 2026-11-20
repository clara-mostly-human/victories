//! gapstats: why two goals of the strategy fail, from replays of a named bot.
//!
//!   gapstats <replay dirs> --bot NAME
//!
//! 1. Heal denial: every health pickup by the opponent while the bot leads (edge 15+, middle
//!    phase). Ten ticks before it: the bot's and the opponent's path length to that pad, their
//!    distance to each other, whether the pad was already up, and the bot's mode.
//! 2. The other pad: games where the opponent took the first power-up. When it did: the bot's
//!    and the opponent's path to the other power pad, ticks until that pad is up, and who took it.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::chebyshev;
use victris_engine::map::PadKind;
use victris_engine::replay::parse_replay;
use victris_engine::state::State;
use victris_engine::strategy;

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
    let bot = opt("--bot").expect("--bot NAME");
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bot" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let mut heals = 0usize;
    let (mut we_nearer, mut pad_up, mut sum_ours, mut sum_theirs, mut sum_d) = (0usize, 0usize, 0.0, 0.0, 0.0);
    let mut modes = [0usize; 5];
    let (mut missed_first, mut other_ours, mut other_theirs, mut other_none) = (0usize, 0usize, 0usize, 0usize);
    let (mut o_sum_ours, mut o_sum_theirs, mut o_sum_wait, mut o_nearer) = (0.0, 0.0, 0.0, 0usize);
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        let o = 1 - me;
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let mut hist: Vec<State> = Vec::new();
        let mut power_takers: Vec<(usize, usize)> = Vec::new(); // (who, pad index)
        let mut other_watch: Option<usize> = None;
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            hist.push(s);
            let before = s;
            e.step(&mut s, &t.turns);
            for (i, d) in e.map.pads.iter().enumerate() {
                if !(before.pads[i].available && !s.pads[i].available) {
                    continue;
                }
                let who = if s.fighters[me].pos == d.pos { me } else { o };
                if d.kind == PadKind::Health && who == o && strategy::phase(e, &before) == 1 && strategy::edge(&before, me) >= 15.0 {
                    heals += 1;
                    let k = hist.len().saturating_sub(11);
                    let h = &hist[k];
                    let (po, pt) = (strategy::path(e, d.pos, h.fighters[me].pos), strategy::path(e, d.pos, h.fighters[o].pos));
                    sum_ours += po as f64;
                    sum_theirs += pt as f64;
                    sum_d += chebyshev(h.fighters[me].pos, h.fighters[o].pos) as f64;
                    we_nearer += (po < pt) as usize;
                    pad_up += h.pads[i].available as usize;
                    modes[strategy::mode_index(strategy::choose(e, h, me))] += 1;
                }
                if d.kind == PadKind::Power {
                    power_takers.push((who, i));
                    if power_takers.len() == 1 && who == o {
                        missed_first += 1;
                        if let Some((j, od)) = e.map.pads.iter().enumerate().find(|(j, pd)| pd.kind == PadKind::Power && *j != i) {
                            let (po, pt) = (strategy::path(e, od.pos, s.fighters[me].pos), strategy::path(e, od.pos, s.fighters[o].pos));
                            o_sum_ours += po as f64;
                            o_sum_theirs += pt as f64;
                            o_nearer += (po < pt) as usize;
                            o_sum_wait += if s.pads[j].available { 0.0 } else { (s.pads[j].respawn_tick - s.tick).max(0) as f64 };
                            other_watch = Some(j);
                        }
                    } else if let Some(j) = other_watch {
                        if i == j {
                            if who == me { other_ours += 1 } else { other_theirs += 1 }
                            other_watch = None;
                        }
                    }
                }
            }
        }
        if other_watch.is_some() {
            other_none += 1;
        }
    }
    let hn = heals.max(1) as f64;
    println!("heal denial: {heals} opponent heals while {bot} led in the middle game; 10 ticks before: we were nearer the pad {:.0}% of the time (path {:.1} vs their {:.1}), {:.1} tiles apart, pad already up {:.0}%; our mode then: {}", 100.0 * we_nearer as f64 / hn, sum_ours / hn, sum_theirs / hn, sum_d / hn, 100.0 * pad_up as f64 / hn, (0..5).map(|k| format!("{} {}", strategy::MODE_NAMES[k], modes[k])).collect::<Vec<_>>().join(", "));
    let mn = missed_first.max(1) as f64;
    println!("other pad: missed the first power-up in {missed_first} games; then our path to the other pad {:.1} vs theirs {:.1} (we nearer {:.0}%), it was up in {:.1} ticks on average; it went to us {other_ours}, to them {other_theirs}, to nobody before the game ended {other_none}", o_sum_ours / mn, o_sum_theirs / mn, 100.0 * o_nearer as f64 / mn, o_sum_wait / mn);
}
