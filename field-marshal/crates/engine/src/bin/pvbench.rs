//! pvbench: the learned search's speed at the size a live bot runs it, and a check that a change
//! to the search leaves every decision exactly as it was.
//!
//!   pvbench --net NET.json [--map arena-21] [--lines 80000] [--ponder 32000] [--nodes 150000]
//!           [--lead 256] [--ticks 60] [--seed 1] [--quiet]
//!
//! One game of the search against itself, run the way a live bot runs it: every tick the tree
//! kept from the tick before is moved on to the new position (`Search::advance_to`), --lines
//! lines are searched, both fighters play their most visited turn, and --ponder more lines are
//! searched under fighter 0's turn before the next tick (`Search::ponder`). No clock decides
//! anything, so two runs are the same line for line.
//!
//! Each tick prints the turns played, the lines through the root (kept ones included), the
//! positions in the tree, and a sum over every root turn's visits and total result, bit for
//! bit. The last line gives the sum over the whole run and the lines searched a second. A
//! change that only makes the search faster must leave every sum as it was.

use victris_engine::pvnet::PvNet;
use victris_engine::pvsearch::{self, PvConfig};
use victris_engine::{Engine, RawTurn};

fn mix(h: &mut u64, v: u64) {
    *h ^= v;
    *h = h.wrapping_mul(0x0000_0100_0000_01b3);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let map = opt("--map").unwrap_or_else(|| "arena-21".to_string());
    let num = |name: &str, d: u64| opt(name).and_then(|s| s.parse().ok()).unwrap_or(d);
    let (lines, ponder, nodes, lead, ticks, seed) = (num("--lines", 80_000) as u32, num("--ponder", 32_000) as u32, num("--nodes", 150_000) as usize, num("--lead", 256) as u32, num("--ticks", 60), num("--seed", 1));
    let quiet = args.iter().any(|a| a == "--quiet");
    let net = PvNet::load(std::path::Path::new(&opt("--net").expect("--net NET.json"))).expect("cannot read the network");
    let e = Engine::shipped(&map);
    let mut s = e.new_match(seed as i64);
    let mut kept: Option<(pvsearch::Tree, victris_engine::state::State)> = None;
    let (mut total, mut searched) = (0xcbf2_9ce4_8422_2325u64, 0u64);
    let mut spent = std::time::Duration::ZERO;
    for _ in 0..ticks {
        if s.result.is_some() {
            break;
        }
        let started = std::time::Instant::now();
        let mut search = pvsearch::Search::new(&e, &net, PvConfig { lead_at: lead, max_nodes: nodes, ..Default::default() });
        if let Some((tree, was)) = kept.take() {
            search.start_from(tree);
            search.advance_to(&was, &s);
        }
        let mut done = 0u32;
        while done < lines {
            let chunk = (lines - done).min(500);
            search.iterate(&s, chunk);
            done += chunk;
        }
        let root = search.root();
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let mut pair = [RawTurn::IDLE; 2];
        for x in 0..2 {
            for (i, a) in root[x].iter().enumerate() {
                mix(&mut h, (i as u64) << 32 | a.1 as u64);
                mix(&mut h, a.2.to_bits() as u64);
            }
            if let Some(a) = root[x].iter().max_by_key(|a| a.1) {
                pair[x] = a.0;
            }
        }
        mix(&mut h, search.value().to_bits() as u64);
        let (through, held) = (search.visits(), search.nodes());
        let mut pondered = 0u32;
        while pondered < ponder {
            let chunk = (ponder - pondered).min(200);
            if !search.ponder(&s, 0, &pair[0], chunk) {
                break;
            }
            pondered += chunk;
        }
        mix(&mut h, search.visits() as u64);
        mix(&mut h, search.nodes() as u64);
        spent += started.elapsed();
        searched += (done + pondered) as u64;
        mix(&mut total, h);
        if !quiet {
            println!("t{:3} {:016x} lines {:7} positions {:6} value {:+.4} | {:?} | {:?}", s.tick, h, through, held, search.value(), pair[0], pair[1]);
        }
        let was = s;
        e.step(&mut s, &pair);
        kept = Some((search.into_tree(), was));
    }
    println!("sum {:016x}  {} lines in {:.2} s: {:.0} lines a second", total, searched, spent.as_secs_f64(), searched as f64 / spent.as_secs_f64().max(1e-9));
}
