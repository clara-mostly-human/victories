//! leadstats: what decides games, per bot, from replays: lead conversion, stun windows, dash economy.
//!
//!   leadstats <replay dirs or files (.jsonl or .jsonl.gz)> --bot NAME [--lead 25]
//!
//! For the named bot in every game it played:
//! - lead conversion: games where it first got `--lead` hp ahead, and how many of those it won;
//!   games where it first fell `--lead` hp behind, and how many of those it still won;
//! - stun windows: how often each side was stunned (slammed into a wall), and the damage dealt and
//!   taken while the other side was stunned;
//! - dash economy: dashes a game, the share that went away from the opponent, the share followed
//!   by a slam within 2 ticks; how often it is loaded (dash and repulsor ready) near the opponent
//!   (within 4 tiles), and how often each side holds a slam threat (`valuefeat::slam_threat`).

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::geom::chebyshev;
use victris_engine::replay::parse_replay;
use victris_engine::state::FState;
use victris_engine::turn::DirIn;

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(path).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        entries.sort();
        for e in entries {
            collect(&e, out);
        }
    } else {
        let s = path.to_string_lossy();
        if s.ends_with(".jsonl.gz") || s.ends_with(".jsonl") {
            out.push(path.to_path_buf());
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let bot = opt("--bot").expect("--bot NAME");
    let lead: i32 = opt("--lead").and_then(|s| s.parse().ok()).unwrap_or(25);
    let mut files = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--bot" || a == "--lead" {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            collect(Path::new(a), &mut files);
        }
    }
    let (mut games, mut wins) = (0usize, 0usize);
    let (mut led, mut led_won, mut trailed, mut trailed_won) = (0usize, 0usize, 0usize, 0usize);
    let (mut stunned_us, mut stunned_them, mut dmg_in_our_stun, mut dmg_in_their_stun_dealt) = (0usize, 0usize, 0i64, 0i64);
    let (mut dashes, mut dashes_away, mut dash_slams) = (0usize, 0usize, 0usize);
    let (mut near, mut near_loaded, mut ticks, mut threat_us, mut threat_them) = (0usize, 0usize, 0usize, 0usize, 0usize);
    // after it first leads by `lead`: the next 100 ticks: the trailer's healing, the distance, out of sight
    let (mut after_n, mut after_heal, mut after_dist, mut after_blind, mut after_heals) = (0usize, 0i64, 0.0f64, 0usize, 0usize);
    let mut after_games = 0usize;
    for path in &files {
        let mut text = String::new();
        let ok = if path.to_string_lossy().ends_with(".gz") {
            std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_ok()
        } else {
            std::fs::read_to_string(path).map(|t| text = t).is_ok()
        };
        if !ok {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let Some(me) = replay.bots.iter().position(|b| *b == bot) else { continue };
        let o = 1 - me;
        let e = &replay.engine;
        let mut s = e.new_match(replay.seed);
        let (mut first_lead, mut first_trail) = (false, false);
        let mut slam_at: Vec<i32> = Vec::new();
        let mut lead_tick: Option<i32> = None;
        let mut dash_at: Vec<i32> = Vec::new();
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            let (f, g) = (s.fighters[me], s.fighters[o]);
            let d = f.hp - g.hp;
            if !first_lead && !first_trail {
                if d >= lead {
                    first_lead = true;
                    lead_tick = Some(s.tick);
                    after_games += 1;
                } else if d <= -lead {
                    first_trail = true;
                }
            }
            if f.alive && g.alive {
                ticks += 1;
                if chebyshev(f.pos, g.pos) <= 4 && f.state == FState::Idle {
                    near += 1;
                    near_loaded += victris_engine::valuefeat::loaded(e, &s, me) as usize;
                }
                threat_us += victris_engine::valuefeat::slam_threat(e, &s, me) as usize;
                threat_them += victris_engine::valuefeat::slam_threat(e, &s, o) as usize;
            }
            if let DirIn::Dir(dir) = t.turns[me].dash {
                if f.state == FState::Idle && f.cd_dash == 0 {
                    dashes += 1;
                    dash_at.push(s.tick);
                    let v = victris_engine::geom::DIR_VEC[dir as usize];
                    let to = [f.pos[0] + v[0], f.pos[1] + v[1]];
                    if chebyshev(to, g.pos) > chebyshev(f.pos, g.pos) {
                        dashes_away += 1;
                    }
                }
            }
            if let Some(lt) = lead_tick {
                if s.tick < lt + 100 && f.alive && g.alive {
                    after_n += 1;
                    after_dist += chebyshev(f.pos, g.pos) as f64;
                    after_blind += !victris_engine::observe::can_see(&e.rules, &e.map, f.pos, g.pos) as usize;
                }
            }
            let before = (s.fighters[me].hp, s.fighters[o].hp, s.fighters[me].state, s.fighters[o].state);
            e.step(&mut s, &t.turns);
            let (now_me, now_o) = (s.fighters[me].state, s.fighters[o].state);
            if now_o == FState::Stunned && before.3 != FState::Stunned {
                stunned_them += 1;
                slam_at.push(s.tick);
            }
            if now_me == FState::Stunned && before.2 != FState::Stunned {
                stunned_us += 1;
            }
            if before.2 == FState::Stunned || now_me == FState::Stunned {
                dmg_in_our_stun += (before.0 - s.fighters[me].hp).max(0) as i64;
            }
            if before.3 == FState::Stunned || now_o == FState::Stunned {
                dmg_in_their_stun_dealt += (before.1 - s.fighters[o].hp).max(0) as i64;
            }
            if let Some(lt) = lead_tick {
                if s.tick <= lt + 100 && s.fighters[o].hp > before.1 {
                    after_heal += (s.fighters[o].hp - before.1) as i64;
                    after_heals += 1;
                }
            }
        }
        let Some(w) = s.result.and_then(|r| r.winner) else { continue };
        games += 1;
        let won = (w as usize) == me + 1;
        wins += won as usize;
        if first_lead {
            led += 1;
            led_won += won as usize;
        }
        if first_trail {
            trailed += 1;
            trailed_won += won as usize;
        }
        dash_slams += dash_at.iter().filter(|t| slam_at.iter().any(|s| *s >= **t && *s <= **t + 2)).count();
    }
    let g = games.max(1) as f64;
    println!("{bot}: {games} games, won {:.1}%", 100.0 * wins as f64 / g);
    println!("  first {lead} hp ahead in {led} games, won {:.1}% of them; first {lead} hp behind in {trailed}, won {:.1}%", 100.0 * led_won as f64 / led.max(1) as f64, 100.0 * trailed_won as f64 / trailed.max(1) as f64);
    let an = after_n.max(1) as f64;
    println!("  in the 100 ticks after first leading by {lead} ({after_games} games): the trailer heals {:.1} hp ({:.2} pickups), distance {:.1} tiles, out of sight {:.0}% of ticks", after_heal as f64 / after_games.max(1) as f64, after_heals as f64 / after_games.max(1) as f64, after_dist / an, 100.0 * after_blind as f64 / an);
    println!("  stuns a game: dealt {:.2}, taken {:.2}; damage inside stun windows a game: dealt {:.1}, taken {:.1}", stunned_them as f64 / g, stunned_us as f64 / g, dmg_in_their_stun_dealt as f64 / g, dmg_in_our_stun as f64 / g);
    println!("  dashes a game {:.1}, {:.0}% away from the opponent, {:.0}% followed by a slam; loaded near the opponent {:.0}% of ticks; slam threat held {:.1}% of ticks, faced {:.1}%", dashes as f64 / g, 100.0 * dashes_away as f64 / dashes.max(1) as f64, 100.0 * dash_slams as f64 / dashes.max(1) as f64, 100.0 * near_loaded as f64 / near.max(1) as f64, 100.0 * threat_us as f64 / ticks.max(1) as f64, 100.0 * threat_them as f64 / ticks.max(1) as f64);
}
