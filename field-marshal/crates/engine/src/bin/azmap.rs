//! azmap: what a network thinks of where fighter 0 stands, as a map.
//!
//!   azmap --net NET.json [--tick T] [--other X,Y] [--seed S] [--lines N] [--power K] [--other-power K]
//!
//! The position is the start of a match moved on to tick T with nothing having happened (full
//! hp, everything ready, each pad out only if its first appearance is past), and the other
//! fighter on --other (default: its spawn). Fighter 0 is set down on every floor tile in turn
//! and the network's value for it there is printed, times 100, with no search; with --lines N
//! it is the value at the root of a search of N lines from there instead. Walls are `##`,
//! power pads are marked `P` and health pads `H` after the number, the other fighter is `oo`.
//! --power K: fighter 0 holds the power-up for K more ticks (--other-power: the other does).
//! A look at what the value has learned about ground: for one, whether the tiles by a power
//! pad are worth more just before it appears.

use victris_engine::pvnet::PvNet;
use victris_engine::pvsearch::{PvConfig, Search};
use victris_engine::map::PadKind;
use victris_engine::Engine;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let net = PvNet::load(std::path::Path::new(&arg(&args, "--net").expect("--net NET.json"))).expect("cannot read the network");
    let e = Engine::shipped(&net.map);
    let tick: i32 = arg(&args, "--tick").and_then(|x| x.parse().ok()).unwrap_or(16);
    let seed: i64 = arg(&args, "--seed").and_then(|x| x.parse().ok()).unwrap_or(0);
    let lines: u32 = arg(&args, "--lines").and_then(|x| x.parse().ok()).unwrap_or(0);
    let mut s = e.new_match(seed);
    s.tick = tick;
    for pad in s.pads.iter_mut() {
        pad.available = tick >= pad.respawn_tick;
    }
    if let Some(k) = arg(&args, "--power").and_then(|x| x.parse::<i32>().ok()) {
        s.fighters[0].power_until = tick + k;
    }
    if let Some(k) = arg(&args, "--other-power").and_then(|x| x.parse::<i32>().ok()) {
        s.fighters[1].power_until = tick + k;
    }
    if let Some(o) = arg(&args, "--other") {
        let v: Vec<i32> = o.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        assert!(v.len() == 2, "--other X,Y");
        s.fighters[1].pos = [v[0], v[1]];
    }
    let other = s.fighters[1].pos;
    println!("azmap {}: tick {tick}, {}fighter 0's value (x100) by the tile it stands on; the other fighter on {:?} (oo), fighter 0's spawn {:?}", net.map, if lines > 0 { format!("after a search of {lines} lines ") } else { String::new() }, other, s.fighters[0].pos);
    let (mut sum, mut n) = (0.0f64, 0);
    let mut by_pad: Vec<(f64, usize)> = vec![(0.0, 0); 12];
    for y in 0..e.map.h {
        let mut line = String::new();
        for x in 0..e.map.w {
            let p = [x, y];
            if e.map.is_wall(p) {
                line.push_str("  ## ");
                continue;
            }
            if p == other {
                line.push_str("  oo ");
                continue;
            }
            let mut t = s;
            t.fighters[0].pos = p;
            let v = if lines > 0 {
                let mut search = Search::new(&e, &net, PvConfig::default());
                search.iterate(&t, lines);
                search.value() as f64
            } else {
                net.eval(&e, &t).v0 as f64
            };
            sum += v;
            n += 1;
            let mark = match e.map.pads.iter().find(|q| q.pos == p) {
                Some(q) if q.kind == PadKind::Power => 'P',
                Some(_) => 'H',
                None => ' ',
            };
            let d = e.map.pads.iter().filter(|q| q.kind == PadKind::Power).map(|q| (q.pos[0] - x).abs().max((q.pos[1] - y).abs())).min().unwrap_or(11).min(11) as usize;
            by_pad[d].0 += v;
            by_pad[d].1 += 1;
            line.push_str(&format!("{:4.0}{mark}", 100.0 * v));
        }
        println!("{line}");
    }
    println!("mean over the floor {:+.3}; by tiles from the nearer power pad:{}", sum / n.max(1) as f64, by_pad.iter().enumerate().filter(|(_, b)| b.1 > 0).map(|(d, b)| format!(" {d}: {:+.3}", b.0 / b.1 as f64)).collect::<String>());
}
