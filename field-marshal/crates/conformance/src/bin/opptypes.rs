//! opptypes: sorts the ladder's bots into types by how they play (part 5).
//!
//!   opptypes <replay dir> [--min-games 15] [--k 4] [--out types.json]
//!
//! For every bot with at least `--min-games` games, a style vector from the ticks on which it
//! could act and saw its opponent: mean distance to the opponent; shares of hold, toward,
//! away, sideways and dash among its steps; how often it fires a ready shot, a frostburst in
//! range, a repulsor within 2 tiles; how often it stands within 2 tiles of a power pad that is
//! up or back within 5 ticks. The vectors are scaled feature by feature and sorted into `--k`
//! types by k-means (best of 20 starts). Prints each type's members and what sets it apart;
//! --out writes {"types": [{"members": [...], "centre": {...}}], "bot_type": {bot: type}}.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::Read;
use victris_engine::geom::chebyshev;
use victris_engine::map::PadKind;
use victris_engine::movefeat::labels;
use victris_engine::replay::parse_replay;

const F: usize = 10;
const NAMES: [&str; F] = ["distance", "hold", "toward", "away", "sideways", "dash", "shoots", "frosts", "slams", "on pad"];

#[derive(Default, Clone)]
struct Acc {
    games: usize,
    dist: (f64, f64),
    moves: [f64; 5],
    att: [(f64, f64); 3],
    pad: (f64, f64),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let min_games = opt("--min-games").and_then(|s| s.parse().ok()).unwrap_or(15usize);
    let k = opt("--k").and_then(|s| s.parse().ok()).unwrap_or(4usize).max(1);
    let dir = args.iter().find(|a| !a.starts_with("--") && std::path::Path::new(a).is_dir()).expect("a replay directory");
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let mut acc: HashMap<String, Acc> = HashMap::new();
    for path in &files {
        let mut text = String::new();
        if std::fs::File::open(path).map(|f| GzDecoder::new(f).read_to_string(&mut text)).is_err() {
            continue;
        }
        let Ok(replay) = parse_replay(&text) else { continue };
        let e = &replay.engine;
        let names: Vec<String> = (0..2).map(|i| replay.bots.get(i).cloned().unwrap_or_default()).collect();
        for n in &names {
            acc.entry(n.clone()).or_default().games += 1;
        }
        let mut s = e.new_match(replay.seed);
        for t in &replay.ticks {
            if s.result.is_some() {
                break;
            }
            for x in 0..2 {
                let l = labels(e, &s, x, &t.turns[x]);
                if l.iter().all(|v| v.is_none()) {
                    continue;
                }
                let a = acc.get_mut(&names[x]).unwrap();
                let (f, o) = (&s.fighters[x], &s.fighters[1 - x]);
                a.dist.0 += chebyshev(f.pos, o.pos) as f64;
                a.dist.1 += 1.0;
                if let Some(c) = l[0] {
                    a.moves[c] += 1.0;
                }
                for (j, v) in l[2..5].iter().enumerate() {
                    if let Some(used) = v {
                        a.att[j].0 += *used as f64;
                        a.att[j].1 += 1.0;
                    }
                }
                let near_pad = e.map.pads.iter().enumerate().any(|(i, def)| def.kind == PadKind::Power && chebyshev(f.pos, def.pos) <= 2 && (s.pads[i].available || s.pads[i].respawn_tick - s.tick <= 5));
                a.pad.0 += near_pad as i32 as f64;
                a.pad.1 += 1.0;
            }
            e.step(&mut s, &t.turns);
        }
    }
    let mut bots: Vec<(String, [f64; F], usize)> = Vec::new();
    for (name, a) in &acc {
        if a.games < min_games || a.dist.1 < 200.0 || name == "A" || name == "B" {
            continue;
        }
        let steps: f64 = a.moves.iter().sum::<f64>().max(1.0);
        let r = |p: (f64, f64)| p.0 / p.1.max(1.0);
        let mut v = [0.0f64; F];
        v[0] = r(a.dist);
        for c in 0..5 {
            v[1 + c] = a.moves[c] / steps;
        }
        for j in 0..3 {
            v[6 + j] = r(a.att[j]);
        }
        v[9] = r(a.pad);
        bots.push((name.clone(), v, a.games));
    }
    bots.sort_by(|a, b| a.0.cmp(&b.0));
    if bots.len() < k {
        println!("{} bots with {min_games}+ games: too few for {k} types", bots.len());
        return;
    }
    // scale each feature to mean 0, spread 1
    let n = bots.len() as f64;
    let mut mean = [0.0f64; F];
    let mut sd = [0.0f64; F];
    for (_, v, _) in &bots {
        for j in 0..F {
            mean[j] += v[j] / n;
        }
    }
    for (_, v, _) in &bots {
        for j in 0..F {
            sd[j] += (v[j] - mean[j]).powi(2) / n;
        }
    }
    let z: Vec<[f64; F]> = bots.iter().map(|(_, v, _)| { let mut o = [0.0; F]; for j in 0..F { o[j] = (v[j] - mean[j]) / sd[j].sqrt().max(1e-9); } o }).collect();
    let d2 = |a: &[f64; F], b: &[f64; F]| (0..F).map(|j| (a[j] - b[j]).powi(2)).sum::<f64>();
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rnd = |m: usize| { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as usize % m };
    let mut best: (f64, Vec<usize>, Vec<[f64; F]>) = (f64::MAX, Vec::new(), Vec::new());
    for _ in 0..20 {
        let mut cent: Vec<[f64; F]> = (0..k).map(|_| z[rnd(z.len())]).collect();
        let mut assign = vec![0usize; z.len()];
        for _ in 0..50 {
            for (i, p) in z.iter().enumerate() {
                assign[i] = (0..k).min_by(|a, b| d2(p, &cent[*a]).partial_cmp(&d2(p, &cent[*b])).unwrap()).unwrap();
            }
            for c in 0..k {
                let members: Vec<&[f64; F]> = z.iter().zip(&assign).filter(|(_, a)| **a == c).map(|(p, _)| p).collect();
                if members.is_empty() {
                    cent[c] = z[rnd(z.len())];
                    continue;
                }
                for j in 0..F {
                    cent[c][j] = members.iter().map(|p| p[j]).sum::<f64>() / members.len() as f64;
                }
            }
        }
        let cost: f64 = z.iter().zip(&assign).map(|(p, a)| d2(p, &cent[*a])).sum();
        if cost < best.0 {
            best = (cost, assign, cent);
        }
    }
    let (_, assign, cent) = best;
    let mut types = Vec::new();
    let mut bot_type = serde_json::Map::new();
    for c in 0..k {
        let members: Vec<&(String, [f64; F], usize)> = bots.iter().zip(&assign).filter(|(_, a)| **a == c).map(|(b, _)| b).collect();
        // what sets the type apart: its features most above or below the field
        let mut marks: Vec<(usize, f64)> = (0..F).map(|j| (j, cent[c][j])).collect();
        marks.sort_by(|a, b| b.1.abs().partial_cmp(&a.1.abs()).unwrap());
        let raw = |j: usize| members.iter().map(|m| m.1[j]).sum::<f64>() / members.len().max(1) as f64;
        println!(
            "type {c} ({} bots): {}",
            members.len(),
            marks.iter().take(3).map(|(j, v)| format!("{} {} ({:.2} vs {:.2})", NAMES[*j], if *v > 0.0 { "high" } else { "low" }, raw(*j), mean[*j])).collect::<Vec<_>>().join(", ")
        );
        println!("   {}", members.iter().map(|m| format!("{} ({})", m.0, m.2)).collect::<Vec<_>>().join(", "));
        let mut centre = serde_json::Map::new();
        for j in 0..F {
            centre.insert(NAMES[j].to_string(), serde_json::json!((raw(j) * 1000.0).round() / 1000.0));
        }
        types.push(serde_json::json!({ "members": members.iter().map(|m| m.0.clone()).collect::<Vec<_>>(), "centre": centre }));
        for m in &members {
            bot_type.insert(m.0.clone(), serde_json::json!(c));
        }
    }
    if let Some(out) = opt("--out") {
        std::fs::write(&out, serde_json::to_string_pretty(&serde_json::json!({ "types": types, "bot_type": bot_type })).unwrap()).expect("write --out");
    }
}
