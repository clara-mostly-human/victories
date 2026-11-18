//! `cargo test` proof that the port matches the official engine.
//!
//! * `official_engine_fixture` always runs: 60 games played by the official engine with
//!   random legal and malformed turns, random maps and varied rules (written by
//!   tools/fuzz-oracle.mjs), checked hash by hash.
//! * `ladder_replay_corpus` runs when replays are on disk (the corpus is not in git):
//!   every tick hash of every ladder replay must match. Set VICTRIS_REPLAYS to point
//!   somewhere other than <repo>/replays/corpus.

use flate2::read::GzDecoder;
use std::io::Read;
use std::path::{Path, PathBuf};
use victris_engine::replay::{parse_replay, verify_replay};

fn gunzip(path: &Path) -> String {
    let mut out = String::new();
    GzDecoder::new(std::fs::File::open(path).unwrap()).read_to_string(&mut out).unwrap();
    out
}

/// Verifies one replay text; returns (ticks, hashes checked).
fn check(label: &str, text: &str) -> (usize, usize) {
    let replay = parse_replay(text).unwrap_or_else(|e| panic!("{label}: {e}"));
    match verify_replay(&replay) {
        Ok(v) => (v.ticks, v.hashes_checked),
        Err(d) => panic!("{label}: tick {}: {}\nours: {:?}\ntheirs: {:?}", d.tick, d.message, d.ours, d.theirs),
    }
}

#[test]
fn official_engine_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/official-engine-60-games.jsonl.gz");
    let text = gunzip(&path);
    let mut games = Vec::new();
    let mut start = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("{\"type\":\"header\"") {
            if let Some(s) = start {
                games.push(&text[s..pos]);
            }
            start = Some(pos);
        }
        pos += line.len();
    }
    games.push(&text[start.unwrap()..]);
    assert_eq!(games.len(), 60);
    let mut hashes = 0;
    for (i, g) in games.iter().enumerate() {
        hashes += check(&format!("fixture game {i}"), g).1;
    }
    assert!(hashes > 5000, "only {hashes} hashes checked");
}

#[test]
fn ladder_replay_corpus() {
    let dir = std::env::var_os("VICTRIS_REPLAYS")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../replays/corpus"));
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(d) => d.flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".jsonl.gz")).collect(),
        Err(_) => {
            eprintln!("skipped: no replay corpus at {}", dir.display());
            return;
        }
    };
    files.sort();
    let (mut ticks, mut hashes) = (0, 0);
    for f in &files {
        let (t, h) = check(&f.display().to_string(), &gunzip(f));
        ticks += t;
        hashes += h;
    }
    eprintln!("{} ladder replays, {ticks} ticks, {hashes} hashes matched", files.len());
    assert_eq!(ticks, hashes, "every ladder tick carries a hash");
}
