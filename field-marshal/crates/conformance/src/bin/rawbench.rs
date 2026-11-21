//! rawbench: how long one raw-board judgment takes, encoding and network apart.
//!
//!   rawbench <replay> <net.json>
use flate2::read::GzDecoder;
use std::io::Read;
use std::time::Instant;
use victris_engine::rawnet::{encode, RawNet, IN};
use victris_engine::replay::parse_replay;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut text = String::new();
    GzDecoder::new(std::fs::File::open(&args[0]).unwrap()).read_to_string(&mut text).unwrap();
    let replay = parse_replay(&text).unwrap();
    let net = RawNet::load(std::path::Path::new(&args[1])).expect("net");
    let e = &replay.engine;
    let mut s = e.new_match(replay.seed);
    let mut states = Vec::new();
    for t in &replay.ticks {
        if s.result.is_some() { break; }
        states.push(s);
        e.step(&mut s, &t.turns);
    }
    let n = 200_000usize;
    let mut x = [0.0f32; IN];
    let t0 = Instant::now();
    let mut acc = 0.0f32;
    for i in 0..n { encode(e, &states[i % states.len()], 0, &mut x); acc += x[3]; }
    let enc = t0.elapsed().as_secs_f64() / n as f64 * 1e6;
    let t1 = Instant::now();
    for i in 0..n { x[0] = i as f32 * 1e-6; acc += net.g(&x); }
    let g = t1.elapsed().as_secs_f64() / n as f64 * 1e6;
    let t2 = Instant::now();
    for i in 0..n { acc += net.value0(e, &states[i % states.len()]); }
    let v = t2.elapsed().as_secs_f64() / n as f64 * 1e6;
    let lw = victris_engine::models::value_weights(13);
    let t3 = Instant::now();
    for i in 0..n { acc += victris_engine::valuefeat::value(e, &states[i % states.len()], 0, lw); }
    let l = t3.elapsed().as_secs_f64() / n as f64 * 1e6;
    println!("encode {enc:.2} us, network pass {g:.2} us, full judgment {v:.2} us, linear judgment {l:.2} us ({acc})");
}
