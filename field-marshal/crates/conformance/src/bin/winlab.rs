//! winlab: the bench for the win predictor (`victris_engine::valuenet`). It measures how well
//! a judgment predicts who goes on to win, by stage of the game, and trains new versions.
//!
//!   winlab --cache path [--every 6] [--gen 13] [--top A,B,...] [--ours a,b,...] <replay dirs>...
//!          [--h1 32 --h2 16] [--epochs 12] [--lr 0.001] [--wd 1e-5] [--skill]
//!          [--td 0.9 --td-rounds 2] [--emit path.rs] [--misses N]
//!
//! Every game in the dirs (draws left out) gives a position every `--every` ticks from tick 20
//! while both fighters live: the network's inputs, the hp difference, the current linear
//! judgment (row `--gen`), the tick, and the game's last tick. The examples are kept in
//! `<cache>.bin` and `<cache>.games` and rebuilt when the network's inputs change size (delete
//! the cache after changing what an input means).
//!
//! Games are split by a hash of the file name: one in five held out for the report, one in ten
//! for choosing the epoch, the rest for training. The report gives held-out accuracy and log
//! loss for the network, for the current linear judgment (one fitted scale) and for hp alone
//! (one fitted weight), by stage of the game (quarters of the game's length) and by kind of
//! game: both bots top bots (`--top`), one of ours (`--ours`), self-play (bots named A and B),
//! the rest.
//!
//! --skill gives each fighter its bot's strength as one more input (Bradley-Terry on the
//! training games), so the network does not have to explain the stronger bot's wins by its
//! position; the report then shows the network with the true strengths and with both set equal
//! (what the search would use). --td λ: after the first fit, retrain on λ-returns of the
//! network's own predictions along each training game (the game's result at the end), for
//! `--td-rounds` rounds.

use flate2::read::GzDecoder;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use victris_engine::replay::parse_replay;
use victris_engine::valuenet::{ctx, own, CTX, OWN};

const Q: f32 = 4096.0;
/// Row layout in the store: hp difference (share of max hp), (hp difference + linear judgment) / 32 in hp, fighter 0's inputs, fighter
/// 1's inputs, the position's.
const R: usize = 2 + 2 * OWN + CTX + 2 * EXO + EXC + 2;
/// Column of the teacher's value (see `NO_TEACH`).
const TEACH_COL: usize = R - 2;
/// Last column: the discounted hp fighter 0 went on to gain or lose from here (0.98 a tick,
/// 40 for a kill), / 32: the target the search's linear judgment is fitted to.
const HPC_COL: usize = R - 1;
/// Column `TEACH_COL`: the hp lead plus the teacher's deep-search value for fighter 0, / 32 in hp, or `NO_TEACH`.
const NO_TEACH: i16 = i16::MIN;
/// The teacher run's spacing (`teachdump --every`).
const TEACH_EVERY: usize = 15;
/// Candidate inputs not yet in the engine's network, per fighter: position x, y (2); distance
/// to each pad (4); walls within two tiles (1); hp lost in the last 30 and 100 ticks (2, history).
const EXO: usize = 9;
/// For the position: each pad there or not and ticks to its return (8); ticks to the zone's
/// next shrink and its next radius (2).
const EXC: usize = 10;
/// Blocks of candidate inputs, chosen with --extra: (name, fighter or position, range).
const BLOCKS: [(&str, bool, usize, usize); 6] = [("pos", true, 0, 2), ("pads", true, 2, 6), ("walls", true, 6, 7), ("hist", true, 7, 9), ("cpads", false, 0, 8), ("zone", false, 8, 10)];

fn extra_own(e: &victris_engine::Engine, s: &victris_engine::state::State, x: usize, hp_hist: &[[i32; 2]]) -> [f32; EXO] {
    use victris_engine::geom::chebyshev;
    let f = &s.fighters[x];
    let mut v = [0.0f32; EXO];
    v[0] = f.pos[0] as f32 / 20.0;
    v[1] = f.pos[1] as f32 / 20.0;
    for (k, pad) in e.map.pads.iter().take(4).enumerate() {
        v[2 + k] = chebyshev(f.pos, pad.pos) as f32 / 20.0;
    }
    let mut walls = 0;
    for dx in -2..=2 {
        for dy in -2..=2 {
            if (dx, dy) != (0, 0) && e.map.is_wall([f.pos[0] + dx, f.pos[1] + dy]) {
                walls += 1;
            }
        }
    }
    v[6] = walls as f32 / 24.0;
    let n = hp_hist.len();
    let lost = |back: usize| -> f32 { if n == 0 { 0.0 } else { (hp_hist[n.saturating_sub(back + 1)][x] - f.hp).max(0) as f32 } };
    v[7] = lost(30) / 30.0;
    v[8] = lost(100) / 60.0;
    v
}

fn extra_ctx(e: &victris_engine::Engine, s: &victris_engine::state::State) -> [f32; EXC] {
    let mut v = [0.0f32; EXC];
    for (k, _) in e.map.pads.iter().take(4).enumerate() {
        let p = &s.pads[k];
        v[2 * k] = p.available as i32 as f32;
        v[2 * k + 1] = if p.available { 0.0 } else { ((p.respawn_tick - s.tick).clamp(0, 60)) as f32 / 30.0 };
    }
    if let Some((t, r)) = victris_engine::observe::next_shrink(&e.rules, &e.map, s.tick) {
        v[8] = ((t - s.tick).clamp(0, 100)) as f32 / 50.0;
        v[9] = r.clamp(0, 15) as f32 / 10.0;
    }
    v
}

fn q(v: f32) -> i16 {
    (v.clamp(-7.99, 7.99) * Q).round() as i16
}

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

fn fnv(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[derive(Clone)]
struct Game {
    name: String,
    bots: [String; 2],
    /// the bots' versions, where the replay names them
    ver: [String; 2],
    /// 1 when fighter 0 won
    y: f32,
    last: i32,
}

struct Store {
    games: Vec<Game>,
    game: Vec<u32>,
    tick: Vec<i16>,
    rows: Vec<i16>,
}

impl Store {
    fn len(&self) -> usize {
        self.game.len()
    }
    fn row(&self, i: usize) -> &[i16] {
        &self.rows[i * R..(i + 1) * R]
    }
}

fn one_game(path: &Path, every: usize, gen: usize, playout: u32, teach: Option<&Vec<f32>>) -> Option<(Game, Vec<(i16, Vec<i16>)>)> {
    let mut text = String::new();
    GzDecoder::new(std::fs::File::open(path).ok()?).read_to_string(&mut text).ok()?;
    let replay = parse_replay(&text).ok()?;
    let e = &replay.engine;
    let w = victris_engine::models::value_weights(gen);
    let mut s = e.new_match(replay.seed);
    let mut rows = Vec::new();
    let mut hp_hist: Vec<[i32; 2]> = Vec::new();
    let mut taught = 0usize;
    let mut row_ti: Vec<usize> = Vec::new();
    let mut reward: Vec<f32> = Vec::new();
    for (ti, t) in replay.ticks.iter().enumerate() {
        if s.result.is_some() {
            break;
        }
        hp_hist.push([s.fighters[0].hp, s.fighters[1].hp]);
        let (fa, fb) = (&s.fighters[0], &s.fighters[1]);
        let alive = fa.alive && fb.alive && s.tick >= 20;
        // the positions the teacher valued, by teachdump's own rule
        let teach_here = alive && ti % TEACH_EVERY == 0 && (fa.state == victris_engine::state::FState::Idle || fb.state == victris_engine::state::FState::Idle);
        let tv = match teach {
            Some(v) if teach_here => {
                taught += 1;
                v.get(taught - 1).cloned()
            }
            _ => None,
        };
        if alive && (ti % every == 0 || tv.is_some()) {
            let mut row = Vec::with_capacity(R);
            row.push(q((s.fighters[0].hp - s.fighters[1].hp) as f32 / e.rules.max_hp as f32));
            // --playout N: the inputs are read after N ticks of the search's quiet play-out, the
            // way the search's leaf sees a position
            let mut p = s;
            for _ in 0..playout {
                if p.result.is_some() {
                    break;
                }
                let turns = [victris_engine::mcts::quiet_turn(e, &p, 0, true), victris_engine::mcts::quiet_turn(e, &p, 1, true)];
                e.step(&mut p, &turns);
            }
            let p = &p;
            // what the search's leaf adds up: the hp difference plus the judgment
            row.push(q(((p.fighters[0].hp - p.fighters[1].hp) as f32 + victris_engine::valuefeat::value(e, p, 0, w)) / 32.0));
            row.extend(own(e, p, 0).iter().map(|v| q(*v)));
            row.extend(own(e, p, 1).iter().map(|v| q(*v)));
            row.extend(ctx(e, p).iter().map(|v| q(*v)));
            row.extend(extra_own(e, p, 0, &hp_hist).iter().map(|v| q(*v)));
            row.extend(extra_own(e, p, 1, &hp_hist).iter().map(|v| q(*v)));
            row.extend(extra_ctx(e, p).iter().map(|v| q(*v)));
            // the search's value is the hp it expects to gain from here: add the hp lead it starts from
            row.push(tv.map(|v| q(((s.fighters[0].hp - s.fighters[1].hp) as f32 + v) / 32.0)).unwrap_or(NO_TEACH));
            row.push(0); // hp to come, filled in below
            rows.push((s.tick as i16, row));
            row_ti.push(ti);
        }
        let before = (s.fighters[0].hp.max(0), s.fighters[1].hp.max(0));
        e.step(&mut s, &t.turns);
        let mut r = ((s.fighters[0].hp.max(0) - before.0) - (s.fighters[1].hp.max(0) - before.1)) as f32;
        if !s.fighters[1].alive {
            r += 40.0;
        }
        if !s.fighters[0].alive {
            r -= 40.0;
        }
        reward.push(r);
    }
    // discounted hp to come from each tick (0.98 a tick), as `valuefit` and `ARENA_DUMP` count it
    let mut to_come = vec![0.0f32; reward.len() + 1];
    for k in (0..reward.len()).rev() {
        to_come[k] = reward[k] + 0.98 * to_come[k + 1];
    }
    for ((_, r), ti) in rows.iter_mut().zip(&row_ti) {
        r[HPC_COL] = q(to_come[*ti] / 32.0);
    }
    if let Some(v) = teach {
        if v.len() != taught {
            // the teacher's positions do not line up with this replay: drop its values
            for (_, r) in rows.iter_mut() {
                r[TEACH_COL] = NO_TEACH;
            }
        }
    }
    let winner = s.result?.winner?;
    let bot = |i: usize| replay.bots.get(i).cloned().unwrap_or_default();
    let header: serde_json::Value = text.lines().next().and_then(|l| serde_json::from_str(l).ok()).unwrap_or_default();
    let ver = |i: usize| header["bots"][i]["version"].as_str().unwrap_or("").to_string();
    let name = path.file_name()?.to_string_lossy().to_string();
    Some((Game { name, bots: [bot(0), bot(1)], ver: [ver(0), ver(1)], y: if winner == 1 { 1.0 } else { 0.0 }, last: s.tick as i32 }, rows))
}

fn build(files: &[PathBuf], every: usize, gen: usize, playout: u32, teach: &HashMap<String, Vec<f32>>) -> Store {
    let threads = 3;
    let parts: Vec<Vec<(Game, Vec<(i16, Vec<i16>)>)>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..threads)
            .map(|k| sc.spawn(move || files.iter().skip(k).step_by(threads).filter_map(|p| one_game(p, every, gen, playout, p.file_name().and_then(|n| teach.get(&n.to_string_lossy().to_string())))).collect::<Vec<_>>()))
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut st = Store { games: Vec::new(), game: Vec::new(), tick: Vec::new(), rows: Vec::new() };
    for part in parts {
        for (g, rows) in part {
            let gi = st.games.len() as u32;
            st.games.push(g);
            for (t, r) in rows {
                st.game.push(gi);
                st.tick.push(t);
                st.rows.extend(r);
            }
        }
    }
    st
}

fn save(st: &Store, path: &str) {
    let mut g = String::new();
    for x in &st.games {
        g.push_str(&format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\n", x.name, x.bots[0], x.bots[1], x.y, x.last, x.ver[0], x.ver[1]));
    }
    std::fs::write(format!("{path}.games"), g).expect("write games");
    let mut f = std::io::BufWriter::new(std::fs::File::create(format!("{path}.bin")).expect("create bin"));
    f.write_all(&(R as u32).to_le_bytes()).unwrap();
    f.write_all(&(st.len() as u64).to_le_bytes()).unwrap();
    for i in 0..st.len() {
        f.write_all(&st.game[i].to_le_bytes()).unwrap();
        f.write_all(&st.tick[i].to_le_bytes()).unwrap();
        for v in st.row(i) {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
    }
}

fn load(path: &str) -> Option<Store> {
    let games: Vec<Game> = std::fs::read_to_string(format!("{path}.games"))
        .ok()?
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split('\t').collect();
            let v = |k: usize| p.get(k).map(|x| x.to_string()).unwrap_or_default();
            Some(Game { name: p.first()?.to_string(), bots: [p.get(1)?.to_string(), p.get(2)?.to_string()], ver: [v(5), v(6)], y: p.get(3)?.parse().ok()?, last: p.get(4)?.parse().ok()? })
        })
        .collect();
    let bytes = std::fs::read(format!("{path}.bin")).ok()?;
    if bytes.len() < 12 || u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize != R {
        return None;
    }
    let n = u64::from_le_bytes(bytes[4..12].try_into().ok()?) as usize;
    let stride = 6 + 2 * R;
    if bytes.len() != 12 + n * stride {
        return None;
    }
    let mut st = Store { games, game: Vec::with_capacity(n), tick: Vec::with_capacity(n), rows: Vec::with_capacity(n * R) };
    for i in 0..n {
        let b = &bytes[12 + i * stride..12 + (i + 1) * stride];
        st.game.push(u32::from_le_bytes(b[0..4].try_into().unwrap()));
        st.tick.push(i16::from_le_bytes(b[4..6].try_into().unwrap()));
        for k in 0..R {
            st.rows.push(i16::from_le_bytes(b[6 + 2 * k..8 + 2 * k].try_into().unwrap()));
        }
    }
    Some(st)
}

/// A two-hidden-layer network with sizes chosen at run time, laid out as in `valuenet`.
#[derive(Clone, Copy)]
struct Net {
    n_in: usize,
    h1: usize,
    h2: usize,
}

struct Act {
    z1: Vec<f32>,
    a1: Vec<f32>,
    z2: Vec<f32>,
    a2: Vec<f32>,
}

impl Net {
    fn nw(&self) -> usize {
        self.n_in * self.h1 + self.h1 + self.h1 * self.h2 + self.h2 + self.h2 + 1
    }
    fn act(&self) -> Act {
        Act { z1: vec![0.0; self.h1], a1: vec![0.0; self.h1], z2: vec![0.0; self.h2], a2: vec![0.0; self.h2] }
    }
    fn fwd(&self, w: &[f32], x: &[f32], a: &mut Act) -> f32 {
        let (ni, h1, h2) = (self.n_in, self.h1, self.h2);
        let o_b1 = ni * h1;
        let o_w2 = o_b1 + h1;
        let o_b2 = o_w2 + h1 * h2;
        let o_w3 = o_b2 + h2;
        let o_b3 = o_w3 + h2;
        for j in 0..h1 {
            let row = &w[j * ni..(j + 1) * ni];
            let mut z = w[o_b1 + j];
            for i in 0..ni {
                z += row[i] * x[i];
            }
            a.z1[j] = z;
            a.a1[j] = z.max(0.0);
        }
        for j in 0..h2 {
            let row = &w[o_w2 + j * h1..o_w2 + (j + 1) * h1];
            let mut z = w[o_b2 + j];
            for i in 0..h1 {
                z += row[i] * a.a1[i];
            }
            a.z2[j] = z;
            a.a2[j] = z.max(0.0);
        }
        let mut out = w[o_b3];
        for j in 0..h2 {
            out += w[o_w3 + j] * a.a2[j];
        }
        out
    }
    /// Adds `scale` times the gradient of the output (activations from `fwd` on `x`) to `grad`.
    fn bwd(&self, w: &[f32], x: &[f32], a: &Act, scale: f32, grad: &mut [f32]) {
        let (ni, h1, h2) = (self.n_in, self.h1, self.h2);
        let o_b1 = ni * h1;
        let o_w2 = o_b1 + h1;
        let o_b2 = o_w2 + h1 * h2;
        let o_w3 = o_b2 + h2;
        let o_b3 = o_w3 + h2;
        grad[o_b3] += scale;
        let mut d1 = vec![0.0f32; h1];
        for j in 0..h2 {
            grad[o_w3 + j] += scale * a.a2[j];
            if a.z2[j] <= 0.0 {
                continue;
            }
            let d = scale * w[o_w3 + j];
            grad[o_b2 + j] += d;
            for i in 0..h1 {
                grad[o_w2 + j * h1 + i] += d * a.a1[i];
                d1[i] += d * w[o_w2 + j * h1 + i];
            }
        }
        for j in 0..h1 {
            if a.z1[j] <= 0.0 || d1[j] == 0.0 {
                continue;
            }
            grad[o_b1 + j] += d1[j];
            for i in 0..ni {
                grad[j * ni + i] += d1[j] * x[i];
            }
        }
    }
}

fn sigmoid(z: f32) -> f32 {
    1.0 / (1.0 + (-z).exp())
}

fn logloss(p: f32, y: f32) -> f64 {
    let p = p.clamp(1e-6, 1.0 - 1e-6) as f64;
    -(y as f64 * p.ln() + (1.0 - y as f64) * (1.0 - p).ln())
}

/// The two inputs of the antisymmetric network for example `i`: (mine = fighter 0) and the swap.
fn inputs(st: &Store, i: usize, skill: Option<(f32, f32)>, sel_own: &[usize], sel_ctx: &[usize], xa: &mut Vec<f32>, xb: &mut Vec<f32>) {
    let r = st.row(i);
    let f = |v: i16| v as f32 / Q;
    let a = &r[2..2 + OWN];
    let b = &r[2 + OWN..2 + 2 * OWN];
    let c = &r[2 + 2 * OWN..2 + 2 * OWN + CTX];
    let o = 2 + 2 * OWN + CTX;
    let (ea, eb, ec) = (&r[o..o + EXO], &r[o + EXO..o + 2 * EXO], &r[o + 2 * EXO..o + 2 * EXO + EXC]);
    let side = |x: &mut Vec<f32>, mine: &[i16], ex: &[i16], sk: Option<f32>| {
        x.extend(mine.iter().map(|v| f(*v)));
        x.extend(sel_own.iter().map(|k| f(ex[*k])));
        if let Some(v) = sk {
            x.push(v);
        }
    };
    xa.clear();
    xb.clear();
    side(xa, a, ea, skill.map(|s| s.0));
    side(xa, b, eb, skill.map(|s| s.1));
    side(xb, b, eb, skill.map(|s| s.1));
    side(xb, a, ea, skill.map(|s| s.0));
    for x in [&mut *xa, &mut *xb] {
        x.extend(c.iter().map(|v| f(*v)));
        x.extend(sel_ctx.iter().map(|k| f(ec[*k])));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let num = |name: &str, d: f64| opt(name).and_then(|s| s.parse::<f64>().ok()).unwrap_or(d);
    let cache = opt("--cache").expect("--cache path");
    let every = num("--every", 6.0) as usize;
    let gen = num("--gen", 13.0) as usize;
    let h1 = num("--h1", 32.0) as usize;
    let h2 = num("--h2", 16.0) as usize;
    let epochs = num("--epochs", 12.0) as usize;
    let lr = num("--lr", 0.001) as f32;
    let wd = num("--wd", 1e-5) as f32;
    let use_skill = args.iter().any(|a| a == "--skill");
    let td = opt("--td").and_then(|s| s.parse::<f32>().ok());
    let td_rounds = num("--td-rounds", 2.0) as usize;
    let misses = num("--misses", 0.0) as usize;
    let emit = opt("--emit");
    let list = |name: &str| opt(name).map(|s| s.split(",").filter(|x| !x.is_empty()).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let top = list("--top");
    let ours = { let o = list("--ours"); if o.is_empty() { vec!["field-marshal".to_string(), "foot-soldier".into(), "shock-trooper".into(), "quarter-master".into(), "spy-master".into()] } else { o } };
    let valued = ["--target", "--playout", "--teach-weight", "--teach", "--mix", "--extra", "--frac", "--cache", "--every", "--gen", "--h1", "--h2", "--epochs", "--lr", "--wd", "--td", "--td-rounds", "--misses", "--emit", "--top", "--ours"];
    let mut dirs = Vec::new();
    let mut skip = false;
    for a in &args {
        if skip {
            skip = false;
            continue;
        }
        if valued.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            dirs.push(a.clone());
        }
    }
    let t0 = std::time::Instant::now();
    let st = match load(&cache) {
        Some(st) => st,
        None => {
            let mut files = Vec::new();
            for d in &dirs {
                collect(Path::new(d), &mut files);
            }
            // the same game can be in two dirs (links): keep one
            let mut seen = std::collections::HashSet::new();
            files.retain(|p| seen.insert(p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
            // teacher values: `<dir>/*.txt` from `teachdump --net`, split by replay with the counts
            // in the matching .log ("<replay>: <n> positions so far")
            let mut teach: HashMap<String, Vec<f32>> = HashMap::new();
            if let Some(dir) = opt("--teach") {
                let mut outs: Vec<PathBuf> = std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "txt").unwrap_or(false)).collect()).unwrap_or_default();
                outs.sort();
                for out in outs {
                    let vals: Vec<f32> = std::fs::read_to_string(&out).unwrap_or_default().lines().filter_map(|l| l.split_whitespace().nth(2).and_then(|v| v.parse().ok())).collect();
                    let log = std::fs::read_to_string(out.with_extension("log")).unwrap_or_default();
                    // a line gives this replay's count (", K here") or, from older runs, the
                    // running total of the process, which starts again from zero after a restart
                    let (mut at, mut prev) = (0usize, 0usize);
                    for l in log.lines() {
                        let Some((name, rest)) = l.split_once(": ") else { continue };
                        let Some(n) = rest.split_whitespace().next().and_then(|v| v.parse::<usize>().ok()) else { continue };
                        let here = match rest.split(", ").nth(1).and_then(|h| h.split_whitespace().next()).and_then(|v| v.parse::<usize>().ok()) {
                            Some(k) => k,
                            None => if n >= prev { n - prev } else { n },
                        };
                        prev = n;
                        if at + here > vals.len() {
                            break;
                        }
                        teach.insert(name.to_string(), vals[at..at + here].to_vec());
                        at += here;
                    }
                }
                eprintln!("teacher values for {} replays ({} positions)", teach.len(), teach.values().map(|v| v.len()).sum::<usize>());
            }
            eprintln!("building from {} replays", files.len());
            let st = build(&files, every, gen, num("--playout", 0.0) as u32, &teach);
            save(&st, &cache);
            st
        }
    };
    eprintln!("{} games, {} positions ({:.0}s)", st.games.len(), st.len(), t0.elapsed().as_secs_f64());
    // split by game: 0 = train, 1 = choose the epoch, 2 = report
    let frac = num("--frac", 1.0);
    let split: Vec<u8> = st.games.iter().map(|g| { let h = fnv(&g.name); if h % 10 < 2 { 2 } else if h % 10 == 2 { 1 } else if ((h >> 20) % 1000) as f64 >= frac * 1000.0 { 9 } else { 0 } }).collect();
    let kind = |g: &Game| -> usize {
        let b = &g.bots;
        if b.iter().all(|n| n == "A" || n == "B") {
            2
        } else if b.iter().any(|n| ours.contains(n)) {
            1
        } else if !top.is_empty() && b.iter().all(|n| top.contains(n)) {
            0
        } else {
            3
        }
    };
    let kinds = ["top bots", "ours", "self-play", "the rest"];
    // bot strength: Bradley-Terry on the training games
    let mut rating: HashMap<String, f32> = HashMap::new();
    // --skill-version: a strength per bot version instead of per bot
    let by_version = args.iter().any(|a| a == "--skill-version");
    let key = |g: &Game, k: usize| -> String { if by_version { format!("{}@{}", g.bots[k], g.ver[k]) } else { g.bots[k].clone() } };
    if use_skill {
        let train_games: Vec<&Game> = st.games.iter().zip(&split).filter(|(_, s)| **s == 0).map(|(g, _)| g).collect();
        for g in &train_games {
            for k in 0..2 {
                rating.entry(key(g, k)).or_insert(0.0);
            }
        }
        for _ in 0..300 {
            let mut grad: HashMap<String, f32> = HashMap::new();
            for g in &train_games {
                let (k0, k1) = (key(g, 0), key(g, 1));
                let d = rating[&k0] - rating[&k1];
                let e = g.y - sigmoid(d);
                *grad.entry(k0).or_insert(0.0) += e;
                *grad.entry(k1).or_insert(0.0) -= e;
            }
            for (b, r) in rating.iter_mut() {
                *r += 0.05 * (grad.get(b).cloned().unwrap_or(0.0) - 0.5 * *r);
            }
        }
        let mut rs: Vec<(&String, &f32)> = rating.iter().collect();
        rs.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap());
        eprintln!("strongest: {}", rs.iter().take(8).map(|(n, r)| format!("{n} {r:+.2}")).collect::<Vec<_>>().join(", "));
    }
    let skill_of = |i: usize, equal: bool| -> Option<(f32, f32)> {
        if !use_skill {
            return None;
        }
        if equal {
            return Some((0.0, 0.0));
        }
        let g = &st.games[st.game[i] as usize];
        Some((rating.get(&key(g, 0)).cloned().unwrap_or(0.0), rating.get(&key(g, 1)).cloned().unwrap_or(0.0)))
    };
    let chosen = list("--extra");
    let (mut sel_own, mut sel_ctx) = (Vec::new(), Vec::new());
    for (name, per_fighter, lo, hi) in BLOCKS {
        if chosen.iter().any(|c| c == name) {
            if per_fighter { sel_own.extend(lo..hi) } else { sel_ctx.extend(lo..hi) }
        }
    }
    let side = OWN + sel_own.len() + use_skill as usize;
    let net = Net { n_in: 2 * side + CTX + sel_ctx.len(), h1, h2 };
    let nw = net.nw();
    let idx = |s: u8| -> Vec<usize> { (0..st.len()).filter(|&i| split[st.game[i] as usize] == s).collect() };
    let (train, valid, test) = (idx(0), idx(1), idx(2));
    eprintln!("{} training positions, {} for the epoch, {} held out for the report", train.len(), valid.len(), test.len());
    let y_of = |i: usize| st.games[st.game[i] as usize].y;
    let hp_of = |i: usize| st.row(i)[0] as f32 / Q;
    let lin_of = |i: usize| st.row(i)[1] as f32 / Q;
    // one-weight logistic fits on the training positions (hp alone; the linear judgment)
    let fit1 = |x: &dyn Fn(usize) -> f32| -> f32 {
        let mut k = 0.0f64;
        for _ in 0..100 {
            let (mut g, mut h) = (0.0f64, 1e-9f64);
            for &i in train.iter().step_by(3) {
                let v = x(i) as f64;
                let p = 1.0 / (1.0 + (-(k * v)).exp());
                g += (p - y_of(i) as f64) * v;
                h += p * (1.0 - p) * v * v;
            }
            k -= g / h;
        }
        k as f32
    };
    let k_hp = fit1(&hp_of);
    // hp per share of max hp in the store
    let max_hp = 130.0f32;
    eprintln!("hp alone: {:.4} log-odds per hp ({:.1} hp per unit)", k_hp / max_hp, max_hp / k_hp);
    let k_lin = fit1(&lin_of);
    let prob = |w: &[f32], i: usize, equal: bool, xa: &mut Vec<f32>, xb: &mut Vec<f32>, a: &mut Act| -> f32 {
        inputs(&st, i, skill_of(i, equal), &sel_own, &sel_ctx, xa, xb);
        let za = net.fwd(w, xa, a);
        let zb = net.fwd(w, xb, a);
        sigmoid(za - zb)
    };
    let mut seed = 0x2545f4914f6cdd1du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 40) as f32) / ((1u64 << 24) as f32) - 0.5
    };
    let mut w = vec![0.0f32; nw];
    for (i, v) in w.iter_mut().enumerate() {
        let fan = if i < net.n_in * h1 { net.n_in } else if i < net.n_in * h1 + h1 + h1 * h2 { h1 } else { h2 };
        *v = rnd() * 2.0 * (2.0 / fan as f32).sqrt();
    }
    let teach_of = |i: usize| -> Option<f32> { let v = st.row(i)[TEACH_COL]; if v == NO_TEACH { None } else { Some(v as f32 / Q) } };
    // --target hp: the network predicts the discounted hp to come (in units of 32 hp) instead of
    // the winner, the way the search's linear judgment does; the teacher's value then counts as
    // the deep search's expected gain from here
    let hp_mode = opt("--target").as_deref() == Some("hp");
    let hpc_of = |i: usize| st.row(i)[HPC_COL] as f32 / Q;
    let lead32 = |i: usize| hp_of(i) * 130.0 / 32.0;
    // the linear judgment alone (row `--gen`), / 32 hp, and its one-weight fit to the hp to come
    let jud = |i: usize| (lin_of(i) * 32.0 - hp_of(i) * 130.0) / 32.0;
    let kj = {
        let (mut sxy, mut sxx) = (0.0f64, 1e-9f64);
        for &i in train.iter().step_by(3) {
            sxy += (jud(i) * hpc_of(i)) as f64;
            sxx += (jud(i) * jud(i)) as f64;
        }
        (sxy / sxx) as f32
    };
    // --residual: the network learns what the linear judgment misses (target minus its fit);
    // its verdict is then the judgment plus the network
    let residual = args.iter().any(|a| a == "--residual");
    let base_of = |i: usize| if residual { kj * jud(i) } else { 0.0 };
    let mix = num("--mix", 0.0) as f32;
    // --teach-weight: how much more a position with a teacher value counts in training
    let teach_weight = num("--teach-weight", 1.0) as f32;
    let k_teach = {
        let (mut k, have) = (0.0f64, train.iter().filter(|&&i| teach_of(i).is_some()).count());
        if have > 0 {
            for _ in 0..100 {
                let (mut g, mut h) = (0.0f64, 1e-9f64);
                for &i in &train {
                    let Some(v) = teach_of(i) else { continue };
                    let p = 1.0 / (1.0 + (-(k * v as f64)).exp());
                    g += (p - y_of(i) as f64) * v as f64;
                    h += p * (1.0 - p) * (v * v) as f64;
                }
                k -= g / h;
            }
        }
        eprintln!("teacher values on {have} training positions; fitted scale {:.2} per 32 hp", k);
        k as f32
    };
    let blend = |i: usize, base: f32| -> f32 {
        match teach_of(i) {
            Some(v) if mix > 0.0 && hp_mode => (1.0 - mix) * base + mix * (v - lead32(i)),
            Some(v) if mix > 0.0 => (1.0 - mix) * base + mix * sigmoid(k_teach * v),
            _ => base,
        }
    };
    let mut target: Vec<f32> = (0..st.len()).map(|i| if hp_mode { blend(i, hpc_of(i)) - base_of(i) } else { blend(i, y_of(i)) }).collect();
    let raw = |w: &[f32], i: usize, xa: &mut Vec<f32>, xb: &mut Vec<f32>, a: &mut Act| -> f32 {
        inputs(&st, i, skill_of(i, false), &sel_own, &sel_ctx, xa, xb);
        let za = net.fwd(w, xa, a);
        let zb = net.fwd(w, xb, a);
        za - zb
    };
    let (mut xa, mut xb) = (Vec::with_capacity(net.n_in), Vec::with_capacity(net.n_in));
    let mut a = net.act();
    let mut ab = net.act();
    let valid_ll = |w: &[f32], xa: &mut Vec<f32>, xb: &mut Vec<f32>, a: &mut Act| -> f64 {
        if hp_mode {
            return valid.iter().map(|&i| ((raw(w, i, xa, xb, a) + base_of(i) - hpc_of(i)) as f64).powi(2)).sum::<f64>() / valid.len().max(1) as f64;
        }
        valid.iter().map(|&i| logloss(prob(w, i, false, xa, xb, a), y_of(i))).sum::<f64>() / valid.len().max(1) as f64
    };
    let mut order = train.clone();
    let rounds = 1 + if td.is_some() { td_rounds } else { 0 };
    let mut step = 0i32;
    for round in 0..rounds {
        if round > 0 {
            // λ-returns of the network's own predictions along each training game
            let lam = td.unwrap();
            let p: Vec<f32> = train.iter().map(|&i| prob(&w, i, false, &mut xa, &mut xb, &mut a)).collect();
            let mut gnext = 0.0f32;
            for k in (0..train.len()).rev() {
                let i = train[k];
                let last_of_game = k + 1 == train.len() || st.game[train[k + 1]] != st.game[i];
                gnext = if last_of_game { y_of(i) } else { (1.0 - lam) * p[k + 1] + lam * gnext };
                target[i] = blend(i, gnext);
            }
        }
        let (mut m, mut v2) = (vec![0.0f32; nw], vec![0.0f32; nw]);
        let mut best = (valid_ll(&w, &mut xa, &mut xb, &mut a), w.clone(), 0usize);
        let n_ep = if round == 0 { epochs } else { (epochs / 2).max(2) };
        for epoch in 0..n_ep {
            for i in (1..order.len()).rev() {
                let j = ((rnd() + 0.5) * (i + 1) as f32) as usize % (i + 1);
                order.swap(i, j);
            }
            for batch in order.chunks(256) {
                let mut grad = vec![0.0f32; nw];
                for &i in batch {
                    inputs(&st, i, skill_of(i, false), &sel_own, &sel_ctx, &mut xa, &mut xb);
                    let za = net.fwd(&w, &xa, &mut a);
                    let zb = net.fwd(&w, &xb, &mut ab);
                    let out = if hp_mode { za - zb } else { sigmoid(za - zb) };
                    let d = (out - target[i]) * if teach_of(i).is_some() { teach_weight } else { 1.0 };
                    net.bwd(&w, &xa, &a, d, &mut grad);
                    net.bwd(&w, &xb, &ab, -d, &mut grad);
                }
                step += 1;
                let n = batch.len() as f32;
                for k in 0..nw {
                    let gk = grad[k] / n + wd * w[k];
                    m[k] = 0.9 * m[k] + 0.1 * gk;
                    v2[k] = 0.999 * v2[k] + 0.001 * gk * gk;
                    let mh = m[k] / (1.0 - 0.9f32.powi(step));
                    let vh = v2[k] / (1.0 - 0.999f32.powi(step));
                    w[k] -= lr * mh / (vh.sqrt() + 1e-8);
                }
            }
            let ll = valid_ll(&w, &mut xa, &mut xb, &mut a);
            eprintln!("  round {round} epoch {:>2}: epoch-choice log loss {:.4} ({:.0}s)", epoch + 1, ll, t0.elapsed().as_secs_f64());
            if ll < best.0 {
                best = (ll, w.clone(), epoch + 1);
            }
        }
        w = best.1;
        eprintln!("round {round}: kept epoch {} (log loss {:.4})", best.2, best.0);
    }
    if hp_mode {
        // held-out mean squared error (hp^2) and share of variance explained, by stage: the
        // network, the linear judgment row `--gen` (scaled by one fitted weight), and zero
        let mut cells = [[0.0f64; 5]; 5]; // n, sum y^2, net err^2, judgment err^2, mean y (for variance)
        let mut ys: [Vec<f32>; 5] = Default::default();
        for &i in &test {
            let g = &st.games[st.game[i] as usize];
            let stage = ((st.tick[i] as f32 / g.last.max(1) as f32) * 4.0).floor().clamp(0.0, 3.0) as usize;
            let y = hpc_of(i);
            let pn = raw(&w, i, &mut xa, &mut xb, &mut a) + base_of(i);
            for c in [stage, 4] {
                cells[c][0] += 1.0;
                cells[c][2] += ((pn - y) * 32.0).powi(2) as f64;
                cells[c][3] += ((kj * jud(i) - y) * 32.0).powi(2) as f64;
                ys[c].push(y * 32.0);
            }
        }
        println!("held-out discounted hp to come: mean squared error (hp^2) and share of variance explained (net {h1}x{h2}{}, judgment row {gen} x {kj:.2})", if residual { " on top of the judgment" } else { "" });
        for c in 0..5 {
            let n = cells[c][0].max(1.0);
            let mean = ys[c].iter().map(|v| *v as f64).sum::<f64>() / n;
            let var = ys[c].iter().map(|v| (*v as f64 - mean).powi(2)).sum::<f64>() / n;
            let label = if c < 4 { format!("stage {}/4", c + 1) } else { "all".into() };
            println!("{label:<10} {:>6}  network {:>7.1} ({:>5.1}%)  judgment {:>7.1} ({:>5.1}%)  zero {:>7.1}", cells[c][0] as usize, cells[c][2] / n, 100.0 * (1.0 - cells[c][2] / n / var.max(1e-9)), cells[c][3] / n, 100.0 * (1.0 - cells[c][3] / n / var.max(1e-9)), var);
        }
        if let Some(path) = emit {
            if use_skill || !chosen.is_empty() || h1 != victris_engine::valuenet::H1 || h2 != victris_engine::valuenet::H2 {
                println!("not written: the engine's network has no strength input or other sizes");
            } else {
                let body = w.iter().map(|v| format!("{v:.5}")).collect::<Vec<_>>().join(", ");
                let n = cells[4][0].max(1.0);
                let text = format!("// Written by `winlab --target hp` from {} games: discounted hp to come, held-out squared error {:.1} (judgment row {gen}: {:.1}).\npub const VALUE_HP_NET: [f32; crate::valuenet::NW] = [{body}];\n", st.games.len(), cells[4][2] / n, cells[4][3] / n);
                std::fs::write(&path, text).expect("write the weights");
                println!("weights written to {path}");
            }
        }
        return;
    }
    // the report
    #[derive(Default, Clone, Copy)]
    struct Cell {
        n: f64,
        acc: [f64; 4],
        ll: [f64; 4],
    }
    let mut by_stage = [Cell::default(); 4];
    let mut by_kind = [Cell::default(); 4];
    let mut all = Cell::default();
    let mut miss_list: Vec<(f32, usize)> = Vec::new();
    let mut on_teach = Cell::default();
    // calibration: by how sure the predictor is of the side it favours (50-60%, ..., 90-100%),
    // how many positions and how often that side won; for the network and for hp alone
    let mut calib = [[0.0f64; 4]; 5];
    for &i in &test {
        let g = &st.games[st.game[i] as usize];
        let y = g.y;
        let ps = [
            prob(&w, i, false, &mut xa, &mut xb, &mut a),
            if use_skill { prob(&w, i, true, &mut xa, &mut xb, &mut a) } else { f32::NAN },
            sigmoid(k_lin * lin_of(i)),
            sigmoid(k_hp * hp_of(i)),
        ];
        let stage = ((st.tick[i] as f32 / g.last.max(1) as f32) * 4.0).floor().clamp(0.0, 3.0) as usize;
        for cell in [&mut by_stage[stage], &mut by_kind[kind(g)], &mut all] {
            cell.n += 1.0;
            for (k, p) in ps.iter().enumerate() {
                if p.is_nan() {
                    continue;
                }
                cell.acc[k] += if (*p - 0.5).abs() < 1e-6 { 0.5 } else { ((*p > 0.5) == (y > 0.5)) as i32 as f64 };
                cell.ll[k] += logloss(*p, y);
            }
        }
        for (k, p) in [(0usize, ps[0]), (1, ps[3])] {
            let sure = p.max(1.0 - p);
            let b = (((sure - 0.5) * 10.0).floor() as usize).min(4);
            calib[b][2 * k] += 1.0;
            calib[b][2 * k + 1] += ((p > 0.5) == (y > 0.5)) as i32 as f64;
        }
        if let Some(v) = teach_of(i) {
            let pt = sigmoid(k_teach * v);
            on_teach.n += 1.0;
            for (k, p) in [ps[0], pt, ps[2], ps[3]].iter().enumerate() {
                on_teach.acc[k] += if (*p - 0.5).abs() < 1e-6 { 0.5 } else { ((*p > 0.5) == (y > 0.5)) as i32 as f64 };
                on_teach.ll[k] += logloss(*p, y);
            }
        }
        if misses > 0 {
            miss_list.push(((ps[0] - y).abs(), i));
        }
    }
    let show = |label: &str, c: &Cell| {
        let n = c.n.max(1.0);
        let skill_part = if use_skill { format!("  equal strength {:>5.1}% {:.3}", 100.0 * c.acc[1] / n, c.ll[1] / n) } else { String::new() };
        println!(
            "{label:<16} {:>6}  network {:>5.1}% {:.3}{skill_part}  judgment {:>5.1}% {:.3}  hp alone {:>5.1}% {:.3}",
            c.n as usize,
            100.0 * c.acc[0] / n,
            c.ll[0] / n,
            100.0 * c.acc[2] / n,
            c.ll[2] / n,
            100.0 * c.acc[3] / n,
            c.ll[3] / n
        );
    };
    println!("held-out positions: accuracy and log loss (net {h1}x{h2}{}{}{}{})", if use_skill { ", strengths" } else { "" }, td.map(|l| format!(", td {l}")).unwrap_or_default(), if chosen.is_empty() { String::new() } else { format!(", extra {}", chosen.join("+")) }, if frac < 1.0 { format!(", {:.0}% of training games", frac * 100.0) } else { String::new() });
    for (k, c) in by_stage.iter().enumerate() {
        show(&format!("stage {}/4", k + 1), c);
    }
    for (k, c) in by_kind.iter().enumerate() {
        show(kinds[k], c);
    }
    show("all", &all);
    println!("how sure -> share of positions, and how often the favoured side won (network | hp alone)");
    for (b, c) in calib.iter().enumerate() {
        let n = all.n.max(1.0);
        println!("  {}-{}%: network {:>5.1}% of positions, won {:>5.1}%  |  hp alone {:>5.1}%, won {:>5.1}%", 50 + 10 * b, 60 + 10 * b, 100.0 * c[0] / n, 100.0 * c[1] / c[0].max(1.0), 100.0 * c[2] / n, 100.0 * c[3] / c[2].max(1.0));
    }
    if on_teach.n > 0.0 {
        let n = on_teach.n;
        println!("teacher positions {:>6}  network {:>5.1}% {:.3}  deep search {:>5.1}% {:.3}  judgment {:>5.1}% {:.3}  hp alone {:>5.1}% {:.3}", n as usize, 100.0 * on_teach.acc[0] / n, on_teach.ll[0] / n, 100.0 * on_teach.acc[1] / n, on_teach.ll[1] / n, 100.0 * on_teach.acc[2] / n, on_teach.ll[2] / n, 100.0 * on_teach.acc[3] / n, on_teach.ll[3] / n);
    }
    if misses > 0 {
        miss_list.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        for (err, i) in miss_list.iter().take(misses) {
            let g = &st.games[st.game[*i] as usize];
            println!("miss {:.2}: {} tick {} of {} ({} v {}, fighter 0 {}) hp {:+.2}", err, g.name, st.tick[*i], g.last, g.bots[0], g.bots[1], if g.y > 0.5 { "won" } else { "lost" }, hp_of(*i));
        }
    }
    if let Some(path) = emit {
        if use_skill || !chosen.is_empty() || h1 != victris_engine::valuenet::H1 || h2 != victris_engine::valuenet::H2 {
            println!("not written: the engine's network has no strength input or other sizes");
        } else {
            let body = w.iter().map(|v| format!("{v:.5}")).collect::<Vec<_>>().join(", ");
            let n = all.n.max(1.0);
            let text = format!(
                "// Written by `winlab` from {} games; held-out accuracy {:.1}% (hp alone {:.1}%), log loss {:.4}.\npub const VALUE_NET: [f32; crate::valuenet::NW] = [{body}];\n/// hp per unit of the network's log-odds, from the hp-alone fit (the hp lead worth one unit)\npub const VALUE_NET_HP: f32 = {:.3};\n",
                st.games.len(),
                100.0 * all.acc[0] / n,
                100.0 * all.acc[3] / n,
                all.ll[0] / n,
                max_hp / k_hp
            );
            std::fs::write(&path, text).expect("write the weights");
            println!("weights written to {path}");
        }
    }
}
