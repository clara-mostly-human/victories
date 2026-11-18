//! Fight records: every match our bots play, as the gateway saw it, kept the moment it ends.
//!
//! The gateway sees every frame of every match: the match start (`hello`, with the rules,
//! the map and the opponent), every observation (our fighter, what we can see of the enemy,
//! shots and areas in flight, the events of the tick: hits, pickups, rejections), every turn
//! that went back and who sent it (the bot process, or the gateway for a late or absent
//! process), the end, and the connections it went over. It writes them, as they pass, to
//! `data/fights/<day>/<match>-<bot>.jsonl.gz`, one JSON object per line:
//!
//! - `{"t":"header", "bot", "match", "label", "release", "started_ms", "socket"}`
//! - `{"t":"in", "ts", "socket", "msg": <the ladder's frame, exactly as received>}`
//! - `{"t":"out", "ts", "by": "process" | "gateway", "msg": <the frame sent>}`
//! - `{"t":"note", "ts", "what": "disconnected" | "connected" | "turn_lost", ...}`
//! - `{"t":"footer", "ended_ms", "how", "frames_in", "frames_out", "ticks", "lost_turns", "sockets"}`
//!
//! and indexes it in `fleet.db`'s `fights` table (join `matches` on match and bot for the
//! arm, the playbook and the result). Nothing extra is asked of the ladder.
//!
//! What it is not: a replay the engine can verify. A replay needs both fighters' inputs;
//! this is our side, with the enemy only where we could see it. The ladder's own replays of
//! our matches come through the corpus (`corpus` branch, `fleet collect import`).
//!
//! Matches played before the gateway recorded them are indexed from the bots' own logs
//! (`fleet fights backfill`): our turns, both fighters' health, whether the enemy was in
//! sight, and the end of each match; no observations.

use crate::store::{data_dir, default_db, now_ms, Store};
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::params;
use serde_json::{json, Value};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Below this much free disk no new record is started (the bots' own logs come first).
pub const MIN_FREE_BYTES: u64 = 1_500_000_000;

/// Free bytes on the disk that holds `path`.
pub fn disk_free(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

pub struct FightRecord {
    pub match_id: String,
    bot: String,
    path: PathBuf,
    enc: Option<GzEncoder<BufWriter<std::fs::File>>>,
    started_ms: i64,
    label: String,
    release: String,
    frames_in: u64,
    frames_out: u64,
    raw_bytes: u64,
    ticks: i64,
    lost: i64,
    sockets: Vec<i64>,
}

impl FightRecord {
    /// Starts the record of a match. `None` (and the match is simply not recorded) when
    /// the disk is too full or the file cannot be made.
    pub fn open(root: &Path, bot: &str, match_id: &str, label: &str, release: &str, socket: Option<i64>) -> Option<FightRecord> {
        let base = data_dir(root);
        if disk_free(&base).map(|f| f < MIN_FREE_BYTES).unwrap_or(false) {
            return None;
        }
        let safe: String = match_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        let day = crate::online::utc_stamp()[..10].to_string();
        let dir = base.join("fights").join(day);
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!("{safe}-{bot}.jsonl.gz"));
        let file = std::fs::File::create(&path).ok()?;
        let mut r = FightRecord {
            match_id: match_id.to_string(),
            bot: bot.to_string(),
            path,
            enc: Some(GzEncoder::new(BufWriter::with_capacity(64 * 1024, file), Compression::new(6))),
            started_ms: now_ms(),
            label: label.to_string(),
            release: release.to_string(),
            frames_in: 0,
            frames_out: 0,
            raw_bytes: 0,
            ticks: 0,
            lost: 0,
            sockets: socket.into_iter().collect(),
        };
        let header = json!({ "t": "header", "bot": bot, "match": match_id, "label": label, "release": release, "started_ms": r.started_ms, "socket": socket, "format": 1 });
        r.line(&header.to_string());
        Some(r)
    }

    fn line(&mut self, text: &str) {
        self.raw_bytes += text.len() as u64 + 1;
        if let Some(e) = self.enc.as_mut() {
            if e.write_all(text.as_bytes()).and_then(|_| e.write_all(b"\n")).is_err() {
                // A full disk or a lost file: stop writing, keep playing.
                self.enc = None;
            }
        }
    }

    /// A frame from the ladder, exactly as it came.
    pub fn frame_in(&mut self, socket: i64, raw: &str, tick: Option<i64>) {
        self.frames_in += 1;
        if let Some(t) = tick {
            self.ticks = self.ticks.max(t + 1);
        }
        let line = format!("{{\"t\":\"in\",\"ts\":{},\"socket\":{socket},\"msg\":{raw}}}", now_ms());
        self.line(&line);
    }

    /// A frame sent to the ladder, and who wrote it.
    pub fn frame_out(&mut self, by: &str, raw: &str) {
        self.frames_out += 1;
        let line = format!("{{\"t\":\"out\",\"ts\":{},\"by\":\"{by}\",\"msg\":{raw}}}", now_ms());
        self.line(&line);
    }

    /// Something about the match that is not a frame.
    pub fn note(&mut self, what: &str, detail: Value) {
        if what == "connected" {
            if let Some(s) = detail.get("socket").and_then(Value::as_i64) {
                if !self.sockets.contains(&s) {
                    self.sockets.push(s);
                }
            }
        }
        if what == "turn_lost" {
            self.lost += detail.get("turns").and_then(Value::as_i64).unwrap_or(1);
        }
        let mut v = json!({ "t": "note", "ts": now_ms(), "what": what });
        if let (Some(o), Some(d)) = (v.as_object_mut(), detail.as_object()) {
            for (k, x) in d {
                o.insert(k.clone(), x.clone());
            }
        }
        self.line(&v.to_string());
    }

    /// Closes the record and indexes it. `how`: `end`, `not resumed`, `gateway stopped`.
    pub fn finish(mut self, how: &str, end: Option<&Value>) {
        let ended = now_ms();
        let footer = json!({
            "t": "footer", "ended_ms": ended, "how": how, "frames_in": self.frames_in, "frames_out": self.frames_out,
            "ticks": self.ticks, "lost_turns": self.lost, "sockets": self.sockets,
        });
        self.line(&footer.to_string());
        let ok = match self.enc.take() {
            Some(e) => e.finish().and_then(|mut w| w.flush()).is_ok(),
            None => false,
        };
        let bytes = std::fs::metadata(&self.path).map(|m| m.len() as i64).unwrap_or(0);
        let root = self.path.ancestors().nth(4).map(Path::to_path_buf).unwrap_or_default();
        if let Ok(store) = Store::open(&default_db(&root), Duration::from_secs(3)) {
            let _ = store.conn.execute(
                "INSERT OR REPLACE INTO fights (match_id, bot, source, path, bytes, raw_bytes, frames_in, frames_out, ticks, started_ms, ended_ms, result, reason, how, lost_turns, sockets, label, release, complete)
                 VALUES (?1, ?2, 'gateway', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    self.match_id,
                    self.bot,
                    self.path.display().to_string(),
                    bytes,
                    self.raw_bytes as i64,
                    self.frames_in as i64,
                    self.frames_out as i64,
                    self.ticks,
                    self.started_ms,
                    ended,
                    end.and_then(|e| e.get("result")).and_then(Value::as_str),
                    end.and_then(|e| e.get("reason")).and_then(Value::as_str),
                    how,
                    self.lost,
                    serde_json::to_string(&self.sockets).unwrap_or_default(),
                    self.label,
                    self.release,
                    (ok && how == "end") as i64,
                ],
            );
        }
    }
}

/// Indexes the matches in the bots' own logs for `day` (`YYYY-MM-DD`) that have no record
/// yet: one row per match, pointing at the log that holds its lines. Nothing is copied.
pub fn backfill(root: &Path, store: &Store, bots: &[String], day: &str) -> Result<(usize, usize), String> {
    let mut added = 0;
    let mut seen = 0;
    for bot in bots {
        let dir = root.join("logs").join(bot);
        for name in [format!("{bot}-{day}.jsonl"), format!("{bot}-{day}.jsonl.gz")] {
            let path = dir.join(&name);
            if !path.is_file() {
                continue;
            }
            let text = if name.ends_with(".gz") {
                let mut s = String::new();
                let f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(f), &mut s).map_err(|e| e.to_string())?;
                s
            } else {
                std::fs::read_to_string(&path).map_err(|e| e.to_string())?
            };
            // Per match: its first line's time, its end line, and how many tick lines.
            let mut matches: Vec<(String, i64, Option<Value>, i64)> = Vec::new();
            for l in text.lines() {
                if !(l.contains("\"t\":\"hello\"") || l.contains("\"t\":\"match\"") || l.contains("\"t\":\"tick\"")) {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
                let Some(id) = v.get("match").and_then(Value::as_str) else { continue };
                let ts = v.get("ts").and_then(Value::as_i64).unwrap_or(0);
                let k = match matches.iter().position(|m| m.0 == id) {
                    Some(k) => k,
                    None => {
                        matches.push((id.to_string(), ts, None, 0));
                        matches.len() - 1
                    }
                };
                match v.get("t").and_then(Value::as_str) {
                    Some("tick") => matches[k].3 += 1,
                    Some("match") => matches[k].2 = Some(v),
                    _ => {}
                }
            }
            for (id, started, end, ticks) in matches {
                seen += 1;
                let have: i64 = store.conn.query_row("SELECT COUNT(*) FROM fights WHERE match_id = ?1 AND bot = ?2", params![id, bot], |r| r.get(0)).unwrap_or(0);
                if have > 0 {
                    continue;
                }
                let e = end.as_ref();
                let n = store
                    .conn
                    .execute(
                        "INSERT OR IGNORE INTO fights (match_id, bot, source, path, bytes, raw_bytes, frames_in, frames_out, ticks, started_ms, ended_ms, result, reason, how, lost_turns, sockets, label, release, complete)
                         VALUES (?1, ?2, 'tick log', ?3, NULL, NULL, NULL, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12, ?13)",
                        params![
                            id,
                            bot,
                            path.display().to_string(),
                            ticks,
                            started,
                            e.and_then(|e| e.get("ts")).and_then(Value::as_i64),
                            e.and_then(|e| e.get("result")).and_then(Value::as_str),
                            e.and_then(|e| e.get("reason")).and_then(Value::as_str),
                            if e.is_some() { "end" } else { "no end in the log" },
                            e.and_then(|e| e.get("charge_ms")).and_then(|c| c.get("lost_turns")).and_then(Value::as_i64),
                            e.and_then(|e| e.get("version")).and_then(Value::as_str),
                            Option::<String>::None,
                            e.is_some() as i64,
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                added += n;
            }
        }
    }
    Ok((added, seen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_written_compressed_and_indexed() {
        let root = std::env::temp_dir().join(format!("victris-fights-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        // The store must exist with its tables.
        drop(Store::open(&default_db(&root), Duration::from_secs(2)).unwrap());
        let mut r = FightRecord::open(&root, "alpha", "g123_abc", "v7", "r34", Some(5)).expect("a record");
        r.frame_in(5, r#"{"type":"hello","match_id":"g123_abc"}"#, None);
        for t in 0..300 {
            r.frame_in(5, &format!(r#"{{"type":"observation","tick":{t},"self":{{"hp":100,"budget_ms_left":299000}}}}"#), Some(t));
            r.frame_out("process", &format!(r#"{{"tick":{t},"move":"N"}}"#));
        }
        r.note("disconnected", json!({ "why": "reset" }));
        r.note("connected", json!({ "socket": 6 }));
        r.note("turn_lost", json!({ "cause": "reset", "turns": 4 }));
        let end = json!({ "type": "end", "result": "win", "reason": "last_standing" });
        r.frame_in(6, &end.to_string(), None);
        let path = r.path.clone();
        r.finish("end", Some(&end));
        // Readable, line by line, with every frame.
        let mut text = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap()), &mut text).unwrap();
        let lines: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.first().unwrap()["t"], "header");
        assert_eq!(lines.last().unwrap()["t"], "footer");
        assert_eq!(lines.last().unwrap()["ticks"], 300);
        assert_eq!(lines.last().unwrap()["lost_turns"], 4);
        assert_eq!(lines.last().unwrap()["sockets"], json!([5, 6]));
        assert_eq!(lines.iter().filter(|l| l["t"] == "in").count(), 302);
        let store = Store::open(&default_db(&root), Duration::from_secs(2)).unwrap();
        let (bytes, raw, complete, result): (i64, i64, i64, String) =
            store.conn.query_row("SELECT bytes, raw_bytes, complete, result FROM fights WHERE match_id = 'g123_abc'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert!(bytes > 0 && bytes < raw / 5, "compressed {bytes} of {raw}");
        assert_eq!((complete, result.as_str()), (1, "win"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
