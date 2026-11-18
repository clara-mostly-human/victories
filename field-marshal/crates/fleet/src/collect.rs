//! The replay collector: a slow, polite trickle that grows the replay corpus for good.
//!
//! It watches the ladder's public match list, queues every finished game it sees, and
//! downloads the queue one replay at a time: our own bots' games first, then games of the
//! top bots, then games between strong bots, then the rest. Each replay is re-simulated
//! on the exact engine before it is accepted (every tick hash must match), saved as
//! `replays/corpus/<match id>.jsonl.gz` and indexed in the store with both bots' names,
//! versions and ratings, so a training set is one query (`fleet corpus ...`).
//!
//! Politeness: one request at a time, at most one every five seconds, and a pause that
//! doubles (10 minutes up to 2 hours) whenever the host answers 403 or 429. The bulk
//! download that was blocked on 2026-11-18 took about 700 files in a few minutes, some of
//! them in parallel; this never does either.

use crate::store::{now_ms, Store};
use rusqlite::params;
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const HOST: &str = "https://victrisarena.com";
/// A generic client name: nothing about the account or the project goes into a request.
const AGENT: &str = "replay-collector/1.0";

pub struct Options {
    /// seconds between two requests
    pub gap: Duration,
    /// seconds between two looks at the match list
    pub list_every: Duration,
    /// how many leaderboard places count as "top bots"
    pub top: usize,
    /// both bots at or above this rating: third in line
    pub strong_rating: f64,
    /// stop after this many downloads (for a single pass); `None` runs for good
    pub max_downloads: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Options { gap: Duration::from_secs(5), list_every: Duration::from_secs(300), top: 15, strong_rating: 2000.0, max_downloads: None }
    }
}

pub fn corpus_dir(root: &Path) -> PathBuf {
    root.join("replays").join("corpus")
}

pub fn replay_path(root: &Path, id: &str) -> PathBuf {
    corpus_dir(root).join(format!("{id}.jsonl.gz"))
}

/// Match ids become file names: only the ladder's own alphabet is accepted.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

enum Fetched {
    Ok(Vec<u8>),
    /// 403 or 429: the host wants us to slow down
    Blocked(u16),
    /// 404 or 410: it is not there (any more)
    Gone,
    Failed(String),
}

fn fetch(url: &str) -> Fetched {
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "45", "--retry", "0", "-A", AGENT, "-w", "\n%{http_code}", url])
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => return Fetched::Failed(format!("curl: {e}")),
    };
    if !out.status.success() {
        return Fetched::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    // The status code is the last line; everything before it is the body.
    let cut = out.stdout.iter().rposition(|b| *b == b'\n').unwrap_or(0);
    let code: u16 = String::from_utf8_lossy(&out.stdout[cut..]).trim().parse().unwrap_or(0);
    match code {
        200 => Fetched::Ok(out.stdout[..cut].to_vec()),
        403 | 429 => Fetched::Blocked(code),
        404 | 410 => Fetched::Gone,
        c => Fetched::Failed(format!("HTTP {c}")),
    }
}

fn gunzip(bytes: &[u8]) -> Result<String, String> {
    let mut out = String::new();
    flate2::read::GzDecoder::new(bytes).read_to_string(&mut out).map_err(|e| format!("not a gzip replay: {e}"))?;
    Ok(out)
}

/// Re-simulates a replay on the exact engine. Returns the number of tick hashes that matched.
pub fn verify(text: &str) -> Result<(usize, victris_engine::replay::Replay), String> {
    let replay = victris_engine::replay::parse_replay(text)?;
    // A replay from rules or a map the engine cannot play must not take the collector down.
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| victris_engine::replay::verify_replay(&replay).map(|v| v.hashes_checked).map_err(|d| format!("diverges at tick {}: {}", d.tick, d.message))));
    match checked {
        Ok(Ok(n)) => Ok((n, replay)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the engine could not replay it".into()),
    }
}

/// One game from the ladder's match list.
struct Listed {
    id: String,
    ended: String,
    map: String,
    reason: String,
    final_tick: Option<i64>,
    winner: Option<i64>,
    bots: [(String, String, Option<f64>); 2],
}

fn listed(m: &Value) -> Option<Listed> {
    let id = m.get("id")?.as_str()?.to_string();
    if !safe_id(&id) || m.get("voided").and_then(Value::as_bool).unwrap_or(false) || m.get("ended").map(Value::is_null).unwrap_or(true) {
        return None;
    }
    let bots = m.get("bots")?.as_array()?;
    let seat = |n: i64| -> Option<(String, String, Option<f64>, String)> {
        let b = bots.iter().find(|b| b.get("fighter").and_then(Value::as_i64) == Some(n))?;
        Some((
            b.get("name")?.as_str()?.to_string(),
            b.get("version").and_then(Value::as_str).unwrap_or("").to_string(),
            b.get("rating_before").and_then(Value::as_f64),
            b.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        ))
    };
    let (a, b) = (seat(1)?, seat(2)?);
    let winner = m.get("winner").and_then(Value::as_str).and_then(|w| if w == a.3 { Some(1) } else if w == b.3 { Some(2) } else { None });
    Some(Listed {
        id,
        ended: m.get("ended").and_then(Value::as_str).unwrap_or("").to_string(),
        map: m.get("map").and_then(Value::as_str).unwrap_or("").to_string(),
        reason: m.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
        final_tick: m.get("final_tick").and_then(Value::as_i64),
        winner,
        bots: [(a.0, a.1, a.2), (b.0, b.1, b.2)],
    })
}

fn matches_of(body: &[u8]) -> Vec<Value> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Array(a)) => a,
        Ok(Value::Object(o)) => o.get("matches").and_then(Value::as_array).cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub struct Collector<'a> {
    pub root: PathBuf,
    pub store: &'a Store,
    pub ours: Vec<String>,
    pub opt: Options,
    top: Vec<String>,
    pause: Duration,
    ok_streak: u32,
    pub downloaded: usize,
    pub rejected: usize,
}

/// 0 ours, 1 a top bot plays, 2 both strong, 3 the rest.
fn priority(l: &Listed, ours: &[String], top: &[String], strong: f64) -> i64 {
    let names = [&l.bots[0].0, &l.bots[1].0];
    if names.iter().any(|n| ours.contains(n)) {
        0
    } else if names.iter().any(|n| top.contains(n)) {
        1
    } else if l.bots.iter().all(|b| b.2.unwrap_or(0.0) >= strong) {
        2
    } else {
        3
    }
}

impl<'a> Collector<'a> {
    pub fn new(root: &Path, store: &'a Store, ours: Vec<String>, opt: Options) -> Collector<'a> {
        Collector { root: root.to_path_buf(), store, ours, opt, top: Vec::new(), pause: Duration::ZERO, ok_streak: 0, downloaded: 0, rejected: 0 }
    }

    fn note(&self, text: &str) {
        eprintln!("collector: {text}");
    }

    /// One request, with the gap after it. `None` means the host asked us to back off.
    fn get(&mut self, url: &str) -> Option<Fetched> {
        let r = fetch(url);
        std::thread::sleep(self.opt.gap);
        if let Fetched::Blocked(code) = r {
            self.ok_streak = 0;
            self.pause = if self.pause.is_zero() { Duration::from_secs(600) } else { (self.pause * 2).min(Duration::from_secs(7200)) };
            self.note(&format!("HTTP {code} from the ladder host; pausing {} minutes", self.pause.as_secs() / 60));
            self.store.event(None, "collector_blocked", &serde_json::json!({ "code": code, "pause_s": self.pause.as_secs() }));
            return None;
        }
        // The pause only starts over from ten minutes after a long quiet stretch.
        self.ok_streak += 1;
        if self.ok_streak >= 500 {
            self.pause = Duration::ZERO;
        }
        Some(r)
    }

    fn upsert(&self, l: &Listed) -> bool {
        let pr = priority(l, &self.ours, &self.top, self.opt.strong_rating);
        let on_disk = replay_path(&self.root, &l.id).is_file();
        let n = self
            .store
            .conn
            .execute(
                "INSERT INTO replays (match_id, status, priority, ended, map, reason, final_tick, winner, bot1, version1, rating1, bot2, version2, rating2, ours, added_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                 ON CONFLICT(match_id) DO UPDATE SET ended = ?4, reason = ?6, final_tick = ?7, winner = ?8, rating1 = ?11, rating2 = ?14, ours = ?15,
                   priority = MIN(priority, ?3)
                 WHERE rating1 IS NULL OR ended IS NULL",
                params![
                    l.id,
                    if on_disk { "unverified" } else { "queued" },
                    pr,
                    l.ended,
                    l.map,
                    l.reason,
                    l.final_tick,
                    l.winner,
                    l.bots[0].0,
                    l.bots[0].1,
                    l.bots[0].2,
                    l.bots[1].0,
                    l.bots[1].1,
                    l.bots[1].2,
                    (pr == 0) as i64,
                    now_ms()
                ],
            )
            .unwrap_or(0);
        n > 0
    }

    /// Reads one match list and queues what is new. Returns how many rows were added or filled in.
    fn discover(&mut self, url: &str) -> Option<usize> {
        match self.get(url)? {
            Fetched::Ok(body) => {
                let _ = self.store.conn.execute_batch("BEGIN");
                let n = matches_of(&body).iter().filter_map(listed).filter(|l| self.upsert(l)).count();
                let _ = self.store.conn.execute_batch("COMMIT");
                Some(n)
            }
            Fetched::Failed(e) => {
                self.note(&format!("{url}: {e}"));
                Some(0)
            }
            _ => Some(0),
        }
    }

    fn refresh_top(&mut self) -> Option<()> {
        if let Fetched::Ok(body) = self.get(&format!("{HOST}/api/leaderboard"))? {
            let rows = match serde_json::from_slice::<Value>(&body) {
                Ok(Value::Array(a)) => a,
                Ok(Value::Object(o)) => o.get("bots").and_then(Value::as_array).cloned().unwrap_or_default(),
                _ => Vec::new(),
            };
            let top: Vec<String> = rows.iter().filter_map(|r| r.get("name").and_then(Value::as_str).map(str::to_string)).take(self.opt.top).collect();
            if !top.is_empty() {
                self.top = top;
            }
        }
        Some(())
    }

    /// Downloads, verifies and files the next replay in line. `Some(false)` when the queue is empty.
    fn download_next(&mut self) -> Option<bool> {
        let next: Option<String> = self
            .store
            .conn
            .query_row(
                "SELECT match_id FROM replays WHERE status = 'queued' AND next_try_ms <= ?1 ORDER BY priority, ended DESC LIMIT 1",
                [now_ms()],
                |r| r.get(0),
            )
            .ok();
        let Some(id) = next else { return Some(false) };
        let done = |status: &str, bytes: Option<i64>, ticks: Option<i64>, note: Option<&str>| {
            let _ = self.store.conn.execute(
                "UPDATE replays SET status = ?2, bytes = ?3, ticks = ?4, note = ?5, fetched_ms = ?6, tries = tries + 1 WHERE match_id = ?1",
                params![id, status, bytes, ticks, note, now_ms()],
            );
        };
        let retry = |note: &str| {
            // Try again later, further apart each time; give up after five tries.
            let _ = self.store.conn.execute(
                "UPDATE replays SET tries = tries + 1, note = ?2, next_try_ms = ?3 + 600000 * (1 << MIN(tries, 6)),
                   status = CASE WHEN tries >= 4 THEN 'failed' ELSE 'queued' END WHERE match_id = ?1",
                params![id, note, now_ms()],
            );
        };
        match self.get(&format!("{HOST}/replays/{id}.jsonl.gz"))? {
            Fetched::Ok(bytes) => match gunzip(&bytes).and_then(|text| verify(&text).map(|(n, _)| n)) {
                Ok(hashes) => {
                    let path = replay_path(&self.root, &id);
                    let part = path.with_extension("part");
                    let saved = std::fs::create_dir_all(corpus_dir(&self.root)).and_then(|_| std::fs::write(&part, &bytes)).and_then(|_| std::fs::rename(&part, &path));
                    match saved {
                        Ok(()) => {
                            done("ok", Some(bytes.len() as i64), Some(hashes as i64), None);
                            self.downloaded += 1;
                        }
                        Err(e) => retry(&format!("cannot save: {e}")),
                    }
                }
                Err(e) => {
                    // Kept apart, not in the corpus: a replay the engine disagrees with is a
                    // finding (new rules? a port bug?), not training data.
                    let dir = self.root.join("replays").join("rejected");
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(dir.join(format!("{id}.jsonl.gz")), &bytes);
                    done("bad", Some(bytes.len() as i64), None, Some(&e));
                    self.rejected += 1;
                    self.note(&format!("{id} failed verification: {e}"));
                    self.store.event(None, "replay_rejected", &serde_json::json!({ "match": id, "why": e }));
                }
            },
            Fetched::Gone => done("gone", None, None, Some("not on the host")),
            Fetched::Failed(e) => retry(&e),
            Fetched::Blocked(_) => unreachable!(),
        }
        Some(true)
    }

    /// Runs until stopped, or until `max_downloads` replays are in.
    pub fn run(&mut self, stop: &dyn Fn() -> bool) {
        let mut last_list = std::time::Instant::now() - self.opt.list_every;
        let mut last_top = std::time::Instant::now() - Duration::from_secs(3600);
        let mut rotate = 0usize;
        self.note(&format!("started; one request every {} s", self.opt.gap.as_secs()));
        while !stop() {
            if self.opt.max_downloads.map(|n| self.downloaded >= n).unwrap_or(false) {
                break;
            }
            // `None` from a step means the host pushed back: sleep the pause, then go on.
            let step = (|| -> Option<()> {
                if last_top.elapsed() >= Duration::from_secs(3600) {
                    last_top = std::time::Instant::now();
                    self.refresh_top()?;
                }
                if last_list.elapsed() >= self.opt.list_every {
                    last_list = std::time::Instant::now();
                    let mut found = self.discover(&format!("{HOST}/api/matches?limit=500"))?;
                    // Our own bots' lists reach further back than the shared list, so none of
                    // our games is missed while the collector was off.
                    for bot in self.ours.clone().into_iter().filter(|b| safe_id(b)) {
                        found += self.discover(&format!("{HOST}/api/matches?bot={bot}&limit=200"))?;
                    }
                    // One top bot per round, in turn, for the same reason.
                    if !self.top.is_empty() {
                        let bot = self.top[rotate % self.top.len()].clone();
                        rotate += 1;
                        if safe_id(&bot) {
                            found += self.discover(&format!("{HOST}/api/matches?bot={bot}&limit=200"))?;
                        }
                    }
                    let queued: i64 = self.store.conn.query_row("SELECT COUNT(*) FROM replays WHERE status = 'queued'", [], |r| r.get(0)).unwrap_or(0);
                    self.note(&format!("{found} new games listed; {queued} waiting; {} downloaded and {} rejected since start", self.downloaded, self.rejected));
                }
                if !self.download_next()? {
                    // Nothing to fetch: wait for the next look at the list.
                    std::thread::sleep(Duration::from_secs(5));
                }
                Some(())
            })();
            match step {
                Some(()) => {}
                None => {
                    let until = std::time::Instant::now() + self.pause;
                    while std::time::Instant::now() < until && !stop() {
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        }
        self.note(&format!("stopped; {} downloaded, {} rejected", self.downloaded, self.rejected));
    }
}

/// Brings replays that are already on disk into the index: each is verified, and its bots,
/// map and result are read from the replay itself. Ratings are filled in later if the
/// ladder's lists still show the game.
pub fn index_corpus(root: &Path, store: &Store, ours: &[String]) -> std::io::Result<(usize, usize)> {
    let (mut added, mut bad) = (0, 0);
    let dir = corpus_dir(root);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".jsonl.gz")).collect();
    files.sort();
    for path in files {
        let id = path.file_name().unwrap().to_string_lossy().trim_end_matches(".jsonl.gz").to_string();
        let status: Option<String> = store.conn.query_row("SELECT status FROM replays WHERE match_id = ?1", [&id], |r| r.get(0)).ok();
        if status.as_deref() == Some("ok") || status.as_deref() == Some("bad") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        match gunzip(&bytes).and_then(|t| verify(&t)) {
            Ok((hashes, replay)) => {
                let name = |k: usize| replay.bots.get(k).cloned().unwrap_or_default();
                let is_ours = ours.contains(&name(0)) || ours.contains(&name(1));
                let result = replay.result.as_ref();
                let _ = store.conn.execute(
                    "INSERT INTO replays (match_id, status, priority, map, reason, final_tick, winner, bot1, bot2, ours, added_ms, fetched_ms, bytes, ticks)
                     VALUES (?1, 'ok', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, ?12)
                     ON CONFLICT(match_id) DO UPDATE SET status = 'ok', bytes = ?11, ticks = ?12, fetched_ms = ?10",
                    params![
                        id,
                        if is_ours { 0 } else { 3 },
                        replay.engine.map.name,
                        result.map(|r| r.reason.clone()),
                        result.map(|r| r.final_tick),
                        result.and_then(|r| r.winner),
                        name(0),
                        name(1),
                        is_ours as i64,
                        now_ms(),
                        bytes.len() as i64,
                        hashes as i64
                    ],
                );
                added += 1;
            }
            Err(e) => {
                let _ = store.conn.execute(
                    "INSERT INTO replays (match_id, status, priority, added_ms, note) VALUES (?1, 'bad', 3, ?2, ?3)
                     ON CONFLICT(match_id) DO UPDATE SET status = 'bad', note = ?3",
                    params![id, now_ms(), e],
                );
                bad += 1;
            }
        }
    }
    Ok((added, bad))
}

/// A slice of the corpus, for training and evaluation.
#[derive(Default)]
pub struct Query {
    /// both bots rated at least this
    pub min_rating: Option<f64>,
    /// one of the two bots
    pub bot: Option<String>,
    pub ours: Option<bool>,
    pub since_ms: Option<i64>,
    pub limit: Option<i64>,
}

pub fn corpus(root: &Path, store: &Store, q: &Query) -> rusqlite::Result<Vec<PathBuf>> {
    let mut sql = String::from("SELECT match_id FROM replays WHERE status = 'ok'");
    let mut p: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(r) = q.min_rating {
        p.push(r.into());
        sql.push_str(&format!(" AND rating1 >= ?{0} AND rating2 >= ?{0}", p.len()));
    }
    if let Some(b) = &q.bot {
        p.push(b.clone().into());
        sql.push_str(&format!(" AND (bot1 = ?{0} OR bot2 = ?{0})", p.len()));
    }
    if let Some(o) = q.ours {
        sql.push_str(if o { " AND ours = 1" } else { " AND ours = 0" });
    }
    if let Some(t) = q.since_ms {
        p.push(t.into());
        sql.push_str(&format!(" AND fetched_ms >= ?{}", p.len()));
    }
    sql.push_str(" ORDER BY match_id DESC");
    if let Some(n) = q.limit {
        sql.push_str(&format!(" LIMIT {n}"));
    }
    let mut stmt = store.conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(p), |r| r.get::<_, String>(0))?.map(|id| id.map(|id| replay_path(root, &id))).collect();
    rows
}
