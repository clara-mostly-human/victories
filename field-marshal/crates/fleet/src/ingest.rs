//! Backfill: reads the JSON-lines logs `marshal` writes (`logs/<bot>/*.jsonl`) into the
//! store, so bots that do not report to the store themselves still show up in the numbers.
//! Safe to run again: rows already present are left alone, unfinished ones are completed.

use crate::store::{MatchStart, Store};
use serde_json::Value;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

#[derive(Debug, Default)]
pub struct Ingested {
    pub files: usize,
    pub started: usize,
    pub finished: usize,
}

pub fn ingest_logs(store: &Store, log_dir: &Path, bot: &str, ours: &dyn Fn(&str) -> bool) -> std::io::Result<Ingested> {
    let mut out = Ingested::default();
    let mut files: Vec<_> = std::fs::read_dir(log_dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false)).collect();
    files.sort();
    let _ = store.conn.execute_batch("BEGIN");
    for path in files {
        out.files += 1;
        let mut hellos: HashMap<String, Value> = HashMap::new();
        for line in std::io::BufReader::new(std::fs::File::open(&path)?).lines() {
            let Ok(line) = line else { continue };
            // Tick lines are most of the file; skip them without parsing.
            if !line.contains("\"t\":\"hello\"") && !line.contains("\"t\":\"match\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let id = v.get("match").and_then(Value::as_str).unwrap_or("").to_string();
            let ts = v.get("ts").and_then(Value::as_i64).unwrap_or(0);
            match v.get("t").and_then(Value::as_str) {
                Some("hello") => {
                    hellos.insert(id, v);
                }
                Some("match") => {
                    let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("?").to_string();
                    let hello = hellos.remove(&id);
                    let start = MatchStart {
                        match_id: id.clone(),
                        bot: bot.to_string(),
                        // Lines written before versions were logged belong to v1.
                        version: v.get("version").and_then(Value::as_str).unwrap_or("v1").to_string(),
                        release: None,
                        opponent: text("opponent"),
                        map: text("map"),
                        seat: hello.as_ref().and_then(|h| h.get("seat")).and_then(Value::as_i64),
                        seed: hello.as_ref().and_then(|h| h.get("seed")).and_then(Value::as_i64),
                    };
                    let started = hello.as_ref().and_then(|h| h.get("ts")).and_then(Value::as_i64).unwrap_or(ts);
                    // A row made from the ladder's list knows less than the log (no playbook, no
                    // timings): the log's row takes its place and keeps the ratings.
                    let api: Option<(Option<f64>, Option<f64>)> = store
                        .conn
                        .query_row("SELECT opponent_rating, rating_before FROM matches WHERE match_id = ?1 AND bot = ?2 AND source = 'api'", rusqlite::params![id, bot], |r| Ok((r.get(0)?, r.get(1)?)))
                        .ok();
                    if api.is_some() {
                        let _ = store.conn.execute("DELETE FROM matches WHERE match_id = ?1 AND bot = ?2", rusqlite::params![id, bot]);
                    }
                    let before: i64 = store.conn.query_row("SELECT COUNT(*) FROM matches WHERE match_id = ?1 AND bot = ?2", rusqlite::params![id, bot], |r| r.get(0)).unwrap_or(0);
                    if before == 0 && store.match_started(&start, &text("playbook"), None, ours(&start.opponent), "log", started).is_ok() {
                        out.started += 1;
                    }
                    let open: i64 = store.conn.query_row("SELECT COUNT(*) FROM matches WHERE match_id = ?1 AND bot = ?2 AND result IS NULL", rusqlite::params![id, bot], |r| r.get(0)).unwrap_or(0);
                    if open > 0 && store.match_finished(bot, &v, ts).unwrap_or(0) > 0 {
                        out.finished += 1;
                    }
                    if let Some((theirs, mine)) = api {
                        let _ = store.conn.execute("UPDATE matches SET opponent_rating = ?3, rating_before = ?4 WHERE match_id = ?1 AND bot = ?2", rusqlite::params![id, bot, theirs, mine]);
                    }
                }
                _ => {}
            }
        }
    }
    let _ = store.conn.execute_batch("COMMIT");
    Ok(out)
}
