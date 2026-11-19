//! Telemetry: the gateway's time series, in a store of their own, keyed by socket.
//!
//! A socket is one connection to the ladder. Every new one gets an ID (the `sockets` table
//! in `fleet.db` says which bot it was for, when it opened and closed, and why it closed).
//! What is measured on it goes here, to `data/telemetry.db` (SQLite, WAL mode), apart from
//! `fleet.db` so that a few dozen points a second never contend with a bot writing down a
//! match. The gateway is the one writer; the status page and the command line read.
//!
//! - `series(id, socket_id, metric, unit, keep_raw_days)`: one row per thing measured on a
//!   socket.
//! - `points(series_id, ts_ms, value)`: every measurement, keyed and stored in order of
//!   (series, time), so a stretch of one series is one seek and a run of adjacent rows.
//! - `rollup_1m`, `rollup_5m`, `rollup_1h` `(series_id, bucket_ms, n, sum, min, max)`: one
//!   row per series and bucket, made when the bucket is over. The minute rows are made
//!   from the points, the five-minute rows from the minute rows, the hour rows from those.
//!
//! How long things are kept: raw points for the series' `keep_raw_days` (pings 14 days,
//! the per-turn metrics 2), minute rows 30 days, five-minute rows 180 days, hour rows for
//! good.
//!
//! The metrics: `ping_ms` (the round trip on the socket: at connect, then every 20
//! seconds), and for every turn played on it `charge_ms` (what the ladder billed),
//! `answer_ms` (how long the bot process took) and `wire_ms` (the difference: time on the
//! wire).
//!
//! The writer also carries the gateway's events and its per-bot state into `fleet.db`
//! (`events`, `gateway_state`), in the same once-a-second batch, so no bot thread ever
//! waits on a disk.

use crate::store::{data_dir, default_db, now_ms, Store};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub fn db_path(root: &Path) -> PathBuf {
    data_dir(root).join("telemetry.db")
}

/// Every metric: its unit and how many days its raw points are kept.
/// (Short, for now: the Mac's disk had under 4 GB free on 2026-11-18. The rollups keep the
/// shape of everything for longer.)
pub const METRICS: [(&str, &str, i64); 4] = [("ping_ms", "ms", 14), ("wire_ms", "ms", 2), ("charge_ms", "ms", 2), ("answer_ms", "ms", 2)];

/// The rollups: table, bucket width, what it is made from, and how many days it is kept
/// (0: for good).
const ROLLUPS: [(&str, i64, &str, i64); 3] = [("rollup_1m", 60_000, "points", 30), ("rollup_5m", 300_000, "rollup_1m", 180), ("rollup_1h", 3_600_000, "rollup_5m", 0)];

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS series (
  id INTEGER PRIMARY KEY,
  socket_id INTEGER NOT NULL,
  metric TEXT NOT NULL,
  unit TEXT NOT NULL,
  keep_raw_days INTEGER NOT NULL,
  UNIQUE (socket_id, metric)
);
CREATE TABLE IF NOT EXISTS points (
  series_id INTEGER NOT NULL,
  ts_ms INTEGER NOT NULL,
  value REAL NOT NULL,
  PRIMARY KEY (series_id, ts_ms)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_1m (
  series_id INTEGER NOT NULL,
  bucket_ms INTEGER NOT NULL,
  n INTEGER NOT NULL,
  sum REAL NOT NULL,
  min REAL NOT NULL,
  max REAL NOT NULL,
  PRIMARY KEY (series_id, bucket_ms)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_5m (
  series_id INTEGER NOT NULL,
  bucket_ms INTEGER NOT NULL,
  n INTEGER NOT NULL,
  sum REAL NOT NULL,
  min REAL NOT NULL,
  max REAL NOT NULL,
  PRIMARY KEY (series_id, bucket_ms)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_1h (
  series_id INTEGER NOT NULL,
  bucket_ms INTEGER NOT NULL,
  n INTEGER NOT NULL,
  sum REAL NOT NULL,
  min REAL NOT NULL,
  max REAL NOT NULL,
  PRIMARY KEY (series_id, bucket_ms)
) WITHOUT ROWID;
";

fn open_write(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// The store, for reading. `None` when nothing has been written yet.
pub fn open_read(root: &Path) -> Option<Connection> {
    let path = db_path(root);
    if !path.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(800));
    Some(conn)
}

enum Msg {
    Point { socket_id: i64, metric: &'static str, ts_ms: i64, value: f64 },
    Event { bot: String, kind: String, ts_ms: i64, detail: String },
    State { bot: String, ts_ms: i64, level: String, verdict: String, socket_id: Option<i64>, detail: String },
    SocketClosed { socket_id: i64, ts_ms: i64, reason: String },
    /// write everything received so far, then answer (tests, and a clean stop)
    Flush(Sender<()>),
}

/// The handle the gateway's threads hold. Sending never waits on a disk: a writer thread
/// takes what is sent and writes it in one transaction a second.
pub struct Telemetry {
    tx: Option<Mutex<Sender<Msg>>>,
}

impl Telemetry {
    /// Opens (and creates) the store and starts its writer.
    pub fn start(root: &Path) -> Telemetry {
        let (tx, rx) = std::sync::mpsc::channel();
        let root = root.to_path_buf();
        let started = std::thread::Builder::new().name("telemetry".into()).spawn(move || {
            // Disk writes wait behind the sockets, never the other way round.
            crate::machine::set_thread_class(false);
            writer(root, rx)
        });
        match started {
            Ok(_) => Telemetry { tx: Some(Mutex::new(tx)) },
            Err(e) => {
                eprintln!("telemetry: cannot start the writer ({e}); nothing will be recorded");
                Telemetry { tx: None }
            }
        }
    }

    /// A handle that records nothing.
    pub fn off() -> Telemetry {
        Telemetry { tx: None }
    }

    fn send(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.lock().unwrap_or_else(|p| p.into_inner()).send(m);
        }
    }

    /// One measurement on a socket, now.
    pub fn point(&self, socket_id: i64, metric: &'static str, value: f64) {
        if value.is_finite() && socket_id > 0 {
            self.send(Msg::Point { socket_id, metric, ts_ms: now_ms(), value });
        }
    }

    /// Something that happened to a bot, now (goes to `fleet.db`'s `events`).
    pub fn event(&self, bot: &str, kind: &str, detail: &Value) {
        self.send(Msg::Event { bot: bot.to_string(), kind: kind.to_string(), ts_ms: now_ms(), detail: detail.to_string() });
    }

    /// What is true about a bot (or, as `*`, the gateway) right now (`fleet.db`'s `gateway_state`).
    pub fn state(&self, bot: &str, level: &str, verdict: &str, socket_id: Option<i64>, detail: &Value) {
        self.send(Msg::State { bot: bot.to_string(), ts_ms: now_ms(), level: level.to_string(), verdict: verdict.to_string(), socket_id, detail: detail.to_string() });
    }

    /// A socket has closed, and why.
    pub fn socket_closed(&self, socket_id: i64, reason: &str) {
        if socket_id > 0 {
            self.send(Msg::SocketClosed { socket_id, ts_ms: now_ms(), reason: reason.to_string() });
        }
    }

    /// Waits until everything sent so far is on disk (at most a few seconds).
    pub fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(Msg::Flush(tx));
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }
}

/// Registers a new socket for a bot and returns its ID. This one write is made by the
/// caller itself (a connection is opened a few times an hour, and the ID is needed at
/// once); if the store cannot be reached the socket still gets an ID, from the clock.
pub fn socket_opened(root: &Path, bot: &str, remote: &str, connect_ping_ms: Option<f64>, resumed: bool, replaced: Option<i64>) -> i64 {
    let made = Store::open(&default_db(root), Duration::from_secs(2)).ok().and_then(|s| {
        s.conn
            .execute(
                "INSERT INTO sockets (bot, remote, opened_ms, connect_ping_ms, resumed, replaced_socket) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![bot, remote, now_ms(), connect_ping_ms, resumed as i64, replaced],
            )
            .ok()
            .map(|_| s.conn.last_insert_rowid())
    });
    made.unwrap_or_else(now_ms)
}

fn series_id(conn: &Connection, ids: &mut HashMap<(i64, &'static str), i64>, socket_id: i64, metric: &'static str) -> rusqlite::Result<i64> {
    if let Some(id) = ids.get(&(socket_id, metric)) {
        return Ok(*id);
    }
    let (unit, keep) = METRICS.iter().find(|m| m.0 == metric).map(|m| (m.1, m.2)).unwrap_or(("", 7));
    conn.execute("INSERT OR IGNORE INTO series (socket_id, metric, unit, keep_raw_days) VALUES (?1, ?2, ?3, ?4)", params![socket_id, metric, unit, keep])?;
    let id: i64 = conn.query_row("SELECT id FROM series WHERE socket_id = ?1 AND metric = ?2", params![socket_id, metric], |r| r.get(0))?;
    // Sockets come and go; the cache need not remember the dead ones for ever.
    if ids.len() > 4000 {
        ids.clear();
    }
    ids.insert((socket_id, metric), id);
    Ok(id)
}

/// Writes one batch: the points to the telemetry store, the rest to the main store.
fn write_batch(conn: &mut Connection, main: Option<&Store>, ids: &mut HashMap<(i64, &'static str), i64>, batch: &mut Vec<Msg>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    let mut answers = Vec::new();
    let mut for_main: Vec<Msg> = Vec::new();
    for m in batch.drain(..) {
        match m {
            Msg::Point { socket_id, metric, ts_ms, value } => {
                let id = series_id(&tx, ids, socket_id, metric)?;
                // Two points of one series in the same millisecond: the later one stands.
                tx.execute("INSERT OR REPLACE INTO points (series_id, ts_ms, value) VALUES (?1, ?2, ?3)", params![id, ts_ms, value])?;
            }
            Msg::Flush(answer) => answers.push(answer),
            other => for_main.push(other),
        }
    }
    tx.commit()?;
    if let Some(s) = main {
        // One transaction for everything that goes to the main store this second.
        let _ = s.conn.execute_batch("BEGIN");
        for m in for_main {
            let _ = match m {
                Msg::Event { bot, kind, ts_ms, detail } => s.conn.execute("INSERT INTO events (ts_ms, bot, kind, detail) VALUES (?1, ?2, ?3, ?4)", params![ts_ms, bot, format!("gateway_{kind}"), detail]),
                Msg::State { bot, ts_ms, level, verdict, socket_id, detail } => s.conn.execute(
                    "INSERT INTO gateway_state (bot, ts_ms, level, verdict, socket_id, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(bot) DO UPDATE SET ts_ms = ?2, level = ?3, verdict = ?4, socket_id = ?5, detail = ?6",
                    params![bot, ts_ms, level, verdict, socket_id, detail],
                ),
                Msg::SocketClosed { socket_id, ts_ms, reason } => s.conn.execute("UPDATE sockets SET closed_ms = ?2, close_reason = ?3 WHERE id = ?1 AND closed_ms IS NULL", params![socket_id, ts_ms, reason]),
                _ => Ok(0),
            };
        }
        let _ = s.conn.execute_batch("COMMIT");
    }
    for a in answers {
        let _ = a.send(());
    }
    Ok(())
}

/// Makes one rollup's rows for the buckets from `from` up to but not including `to`
/// (bucket numbers: time divided by the bucket's width), one series at a time so each is
/// a range seek. `source` is the table it is made from.
fn roll_up(conn: &Connection, table: &str, width: i64, source: &str, from: i64, to: i64) -> rusqlite::Result<()> {
    if to <= from {
        return Ok(());
    }
    let (lo, hi) = (from * width, to * width);
    let sql = if source == "points" {
        format!(
            "INSERT OR REPLACE INTO {table} (series_id, bucket_ms, n, sum, min, max)
             SELECT series_id, (ts_ms / {width}) * {width}, COUNT(*), SUM(value), MIN(value), MAX(value)
             FROM points WHERE series_id = ?1 AND ts_ms >= ?2 AND ts_ms < ?3 GROUP BY ts_ms / {width}"
        )
    } else {
        format!(
            "INSERT OR REPLACE INTO {table} (series_id, bucket_ms, n, sum, min, max)
             SELECT series_id, (bucket_ms / {width}) * {width}, SUM(n), SUM(sum), MIN(min), MAX(max)
             FROM {source} WHERE series_id = ?1 AND bucket_ms >= ?2 AND bucket_ms < ?3 GROUP BY bucket_ms / {width}"
        )
    };
    // Only series that can have something in the range: those of sockets not long closed.
    let source_time = if source == "points" { "ts_ms" } else { "bucket_ms" };
    let ids: Vec<i64> = conn
        .prepare(&format!("SELECT id FROM series s WHERE EXISTS (SELECT 1 FROM {source} p WHERE p.series_id = s.id AND p.{source_time} >= ?1 AND p.{source_time} < ?2)"))?
        .query_map(params![lo, hi], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    for id in ids {
        conn.execute(&sql, params![id, lo, hi])?;
    }
    Ok(())
}

/// Drops raw points past their series' keep, and rollup rows past theirs.
fn retire(conn: &Connection) -> rusqlite::Result<()> {
    let now = now_ms();
    // Each series by its own keep (a range delete on the (series, time) key per series).
    let keeps: Vec<i64> = conn.prepare("SELECT DISTINCT keep_raw_days FROM series")?.query_map([], |r| r.get(0))?.filter_map(|r| r.ok()).collect();
    for keep in keeps {
        conn.execute(
            "DELETE FROM points WHERE ts_ms < ?1 AND series_id IN (SELECT id FROM series WHERE keep_raw_days = ?2)",
            params![now - keep * 86_400_000, keep],
        )?;
    }
    for (table, _, _, keep) in ROLLUPS {
        if keep > 0 {
            conn.execute(&format!("DELETE FROM {table} WHERE bucket_ms < ?1"), [now - keep * 86_400_000])?;
        }
    }
    Ok(())
}

fn writer(root: PathBuf, rx: Receiver<Msg>) {
    let path = db_path(&root);
    let mut conn = loop {
        match open_write(&path) {
            Ok(c) => break c,
            Err(e) => {
                eprintln!("telemetry: cannot open {}: {e}; trying again", path.display());
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };
    let mut main: Option<Store> = Store::open(&default_db(&root), Duration::from_secs(2)).ok();
    let mut ids: HashMap<(i64, &'static str), i64> = HashMap::new();
    let mut batch: Vec<Msg> = Vec::new();
    let mut last_write = Instant::now();
    // Where each rollup stops: after its last row, or an hour back on a store that has none.
    let mut rolled_to: Vec<i64> = ROLLUPS
        .iter()
        .map(|(table, width, _, _)| {
            conn.query_row(&format!("SELECT MAX(bucket_ms) / {width} + 1 FROM {table}"), [], |r| r.get::<_, Option<i64>>(0))
                .ok()
                .flatten()
                .unwrap_or(now_ms() / width - 3_600_000 / width)
                .max(now_ms() / width - 2 * 86_400_000 / width)
        })
        .collect();
    let mut last_retire = Instant::now() - Duration::from_secs(3600);
    let mut failures = 0u64;
    loop {
        let gone = match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(m) => {
                if matches!(m, Msg::Flush(_)) {
                    last_write = Instant::now() - Duration::from_secs(60);
                }
                batch.push(m);
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        if !batch.is_empty() && (gone || batch.len() >= 2000 || last_write.elapsed() >= Duration::from_secs(1)) {
            last_write = Instant::now();
            if main.is_none() {
                main = Store::open(&default_db(&root), Duration::from_secs(2)).ok();
            }
            if let Err(e) = write_batch(&mut conn, main.as_ref(), &mut ids, &mut batch) {
                failures += 1;
                if failures <= 3 || failures % 100 == 0 {
                    eprintln!("telemetry: a batch was not written ({e}); {failures} so far");
                }
                // Whatever was in it is lost; a bad batch must not grow without bound.
                batch.clear();
            }
        }
        // A bucket that is over gets its row, the finer rollups first. One bucket of the
        // finest is left as slack for points still on their way.
        for (k, (table, width, source, _)) in ROLLUPS.iter().enumerate() {
            let through = (now_ms() - 60_000) / width;
            if through > rolled_to[k] && roll_up(&conn, table, *width, source, rolled_to[k], through).is_ok() {
                rolled_to[k] = through;
            }
        }
        if last_retire.elapsed() >= Duration::from_secs(3600) {
            last_retire = Instant::now();
            if let Err(e) = retire(&conn) {
                eprintln!("telemetry: old points were not dropped ({e})");
            }
        }
        if gone {
            break;
        }
    }
}

// ---- reading ------------------------------------------------------------------------------------

/// Count, mean, least and greatest of a stretch of a series.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Agg {
    pub n: i64,
    pub mean: f64,
    pub min: f64,
    pub max: f64,
}

/// The newest point of a metric on a socket: (when, value).
pub fn latest(conn: &Connection, socket_id: i64, metric: &str) -> Option<(i64, f64)> {
    conn.query_row(
        "SELECT p.ts_ms, p.value FROM points p JOIN series s ON s.id = p.series_id
         WHERE s.socket_id = ?1 AND s.metric = ?2 ORDER BY p.ts_ms DESC LIMIT 1",
        params![socket_id, metric],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

/// A metric on a socket over the last `window_ms`, from the raw points.
pub fn aggregate(conn: &Connection, socket_id: i64, metric: &str, window_ms: i64) -> Option<Agg> {
    conn.query_row(
        "SELECT COUNT(*), AVG(p.value), MIN(p.value), MAX(p.value) FROM points p JOIN series s ON s.id = p.series_id
         WHERE s.socket_id = ?1 AND s.metric = ?2 AND p.ts_ms >= ?3",
        params![socket_id, metric, now_ms() - window_ms],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<f64>>(3)?)),
    )
    .ok()
    .and_then(|(n, mean, min, max)| Some(Agg { n, mean: mean?, min: min?, max: max? }))
}

/// A metric over several sockets (a bot's, in the order it held them) as one mean per
/// minute for the last `minutes` minutes, oldest first; `None` where a minute has no
/// point. Finished minutes come from the minute rollup, the last two from the raw points.
pub fn trend(conn: &Connection, socket_ids: &[i64], metric: &str, minutes: i64) -> Vec<Option<f64>> {
    let now_min = now_ms() / 60_000;
    let first = now_min - minutes + 1;
    let mut sums: Vec<(f64, f64)> = vec![(0.0, 0.0); minutes.max(0) as usize];
    for socket in socket_ids {
        let Ok(id) = conn.query_row("SELECT id FROM series WHERE socket_id = ?1 AND metric = ?2", params![socket, metric], |r| r.get::<_, i64>(0)) else { continue };
        let mut add = |minute: i64, n: f64, sum: f64| {
            let k = minute - first;
            if k >= 0 && (k as usize) < sums.len() {
                sums[k as usize].0 += n;
                sums[k as usize].1 += sum;
            }
        };
        if let Ok(mut q) = conn.prepare("SELECT bucket_ms / 60000, n, sum FROM rollup_1m WHERE series_id = ?1 AND bucket_ms >= ?2 AND bucket_ms < ?3") {
            if let Ok(rows) = q.query_map(params![id, first * 60_000, (now_min - 1) * 60_000], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))) {
                for (m, n, s) in rows.flatten() {
                    add(m, n, s);
                }
            }
        }
        if let Ok(mut q) = conn.prepare("SELECT ts_ms / 60000, COUNT(*), SUM(value) FROM points WHERE series_id = ?1 AND ts_ms >= ?2 GROUP BY 1") {
            if let Ok(rows) = q.query_map(params![id, (now_min - 1) * 60_000], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?))) {
                for (m, n, s) in rows.flatten() {
                    add(m, n, s);
                }
            }
        }
    }
    sums.iter().map(|(n, s)| if *n > 0.0 { Some(s / n) } else { None }).collect()
}

// ---- the gateway's state and a bot's network, for the page and the command line ---------------

/// One row of `fleet.db`'s `gateway_state`: what is true about a bot (or, as `*`, the
/// gateway) and when that was written.
#[derive(Clone, Debug)]
pub struct GatewayState {
    pub bot: String,
    pub ts_ms: i64,
    pub level: String,
    pub verdict: String,
    pub socket_id: Option<i64>,
    pub detail: Value,
}

/// Every row of `gateway_state` (from the main store's connection).
pub fn gateway_states(main: &Connection) -> Vec<GatewayState> {
    let Ok(mut q) = main.prepare("SELECT bot, ts_ms, level, verdict, socket_id, detail FROM gateway_state ORDER BY bot") else { return Vec::new() };
    let rows = q.query_map([], |r| {
        Ok(GatewayState { bot: r.get(0)?, ts_ms: r.get(1)?, level: r.get(2)?, verdict: r.get(3)?, socket_id: r.get(4)?, detail: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null) })
    });
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// One socket, from `fleet.db`'s `sockets`.
#[derive(Clone, Debug, Default)]
pub struct SocketRow {
    pub id: i64,
    pub remote: Option<String>,
    pub opened_ms: i64,
    pub closed_ms: Option<i64>,
    pub close_reason: Option<String>,
    pub connect_ping_ms: Option<f64>,
    pub resumed: bool,
}

/// Everything about a bot's sockets: the one it has now, measured over the last minute,
/// five minutes and hour, and how often it has been given a new one.
#[derive(Clone, Debug, Default)]
pub struct Network {
    pub socket: Option<SocketRow>,
    /// the newest ping and wire time on the current socket: (when, value)
    pub ping_now: Option<(i64, f64)>,
    pub wire_now: Option<(i64, f64)>,
    /// on the current socket over the last 1 minute, 5 minutes, 1 hour
    pub ping: [Option<Agg>; 3],
    pub wire: [Option<Agg>; 3],
    pub charge: [Option<Agg>; 3],
    pub answer: [Option<Agg>; 3],
    /// the bot's ping, one mean a minute over the last hour, across all its sockets
    pub trend: Vec<Option<f64>>,
    /// sockets opened in the last hour, and how many of those the gateway gave up (rotated)
    pub sockets_hour: i64,
    pub rotations_hour: i64,
    /// the last rotation: when, and why
    pub last_rotation: Option<(i64, String)>,
    /// the last sockets, newest first
    pub recent: Vec<SocketRow>,
    /// turns lost in the last 24 hours, as the gateway saw them: (socket, cause, arm or
    /// playbook, turns). Causes: `reset` (the connection dropped mid-match and the ladder
    /// played those turns idle until it resumed), `wire` (the turn left in time and arrived
    /// late), `late answer` (the bot process took too long; the gateway sent its fallback),
    /// `no process` (no bot process was attached).
    pub lost: Vec<(Option<i64>, String, String, i64)>,
}

pub const WINDOWS: [(i64, &str); 3] = [(60_000, "1 min"), (300_000, "5 min"), (3_600_000, "1 h")];

fn socket_rows(main: &Connection, sql: &str, p: impl rusqlite::Params) -> Vec<SocketRow> {
    let Ok(mut q) = main.prepare(sql) else { return Vec::new() };
    let rows = q.query_map(p, |r| {
        Ok(SocketRow { id: r.get(0)?, remote: r.get(1)?, opened_ms: r.get(2)?, closed_ms: r.get(3)?, close_reason: r.get(4)?, connect_ping_ms: r.get(5)?, resumed: r.get::<_, i64>(6)? != 0 })
    });
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// A bot's network. `current` is the socket the gateway says it is on; without one, the
/// newest socket still open in the record.
pub fn network(main: &Connection, tel: Option<&Connection>, bot: &str, current: Option<i64>) -> Network {
    const COLS: &str = "id, remote, opened_ms, closed_ms, close_reason, connect_ping_ms, resumed";
    let now = now_ms();
    let mut n = Network::default();
    n.recent = socket_rows(main, &format!("SELECT {COLS} FROM sockets WHERE bot = ?1 ORDER BY id DESC LIMIT 12"), params![bot]);
    n.socket = match current {
        Some(id) => socket_rows(main, &format!("SELECT {COLS} FROM sockets WHERE id = ?1"), params![id]).pop(),
        None => n.recent.iter().find(|s| s.closed_ms.is_none()).cloned(),
    };
    n.sockets_hour = main.query_row("SELECT COUNT(*) FROM sockets WHERE bot = ?1 AND opened_ms >= ?2", params![bot, now - 3_600_000], |r| r.get(0)).unwrap_or(0);
    n.rotations_hour = main
        .query_row("SELECT COUNT(*) FROM sockets WHERE bot = ?1 AND closed_ms >= ?2 AND close_reason LIKE 'rotated%'", params![bot, now - 3_600_000], |r| r.get(0))
        .unwrap_or(0);
    n.lost = main
        .prepare(
            "SELECT json_extract(e.detail, '$.socket'), COALESCE(json_extract(e.detail, '$.cause'), '?'), COALESCE(m.arm, m.playbook, 'no game recorded'), SUM(COALESCE(json_extract(e.detail, '$.turns'), 1))
             FROM events e LEFT JOIN matches m ON m.match_id = json_extract(e.detail, '$.match') AND m.bot = e.bot
             WHERE e.bot = ?1 AND e.kind = 'gateway_turn_lost' AND e.ts_ms >= ?2 GROUP BY 1, 2, 3 ORDER BY 1 DESC",
        )
        .and_then(|mut q| q.query_map(params![bot, now - 86_400_000], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).map(|rows| rows.filter_map(|r| r.ok()).collect()))
        .unwrap_or_default();
    n.last_rotation = main
        .query_row("SELECT closed_ms, close_reason FROM sockets WHERE bot = ?1 AND close_reason LIKE 'rotated%' ORDER BY closed_ms DESC LIMIT 1", params![bot], |r| Ok((r.get(0)?, r.get(1)?)))
        .ok();
    let Some(tel) = tel else { return n };
    if let Some(sock) = &n.socket {
        n.ping_now = latest(tel, sock.id, "ping_ms");
        n.wire_now = latest(tel, sock.id, "wire_ms");
        for (k, (w, _)) in WINDOWS.iter().enumerate() {
            n.ping[k] = aggregate(tel, sock.id, "ping_ms", *w);
            n.wire[k] = aggregate(tel, sock.id, "wire_ms", *w);
            n.charge[k] = aggregate(tel, sock.id, "charge_ms", *w);
            n.answer[k] = aggregate(tel, sock.id, "answer_ms", *w);
        }
    }
    let hour_ids: Vec<i64> = main
        .prepare("SELECT id FROM sockets WHERE bot = ?1 AND (closed_ms IS NULL OR closed_ms >= ?2) ORDER BY id")
        .and_then(|mut q| q.query_map(params![bot, now - 3_600_000], |r| r.get(0)).map(|rows| rows.filter_map(|r| r.ok()).collect()))
        .unwrap_or_default();
    n.trend = trend(tel, &hour_ids, "ping_ms", 60);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("victris-telemetry-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        dir
    }

    #[test]
    fn points_are_kept_per_socket_and_the_main_store_gets_the_rest() {
        let root = temp_root("write");
        let a = socket_opened(&root, "alpha", "10.0.0.1", Some(41.0), false, None);
        let b = socket_opened(&root, "alpha", "10.0.0.2", Some(190.0), true, Some(a));
        assert!(b > a && a > 0);
        let t = Telemetry::start(&root);
        for k in 0..50 {
            t.point(a, "ping_ms", 40.0 + k as f64);
            std::thread::sleep(Duration::from_millis(1));
        }
        t.point(b, "ping_ms", 190.0);
        t.point(b, "wire_ms", 300.0);
        t.event("alpha", "connected", &serde_json::json!({ "socket": a }));
        t.state("alpha", "ok", "playing", Some(b), &serde_json::json!({ "rotation": { "scheduled": false } }));
        t.state("alpha", "warn", "stalled 130 s", Some(b), &serde_json::json!({}));
        t.socket_closed(a, "replaced: slow");
        t.flush();
        let conn = open_read(&root).expect("the store exists");
        // Each socket has its own series.
        assert_eq!(latest(&conn, a, "ping_ms").map(|x| x.1), Some(89.0));
        assert_eq!(latest(&conn, b, "ping_ms").map(|x| x.1), Some(190.0));
        assert!(latest(&conn, a, "wire_ms").is_none());
        let g = aggregate(&conn, a, "ping_ms", 60_000).unwrap();
        assert_eq!((g.n, g.min, g.max), (50, 40.0, 89.0));
        assert!((g.mean - 64.5).abs() < 1e-9);
        // The bot's trend runs over both sockets.
        let tr = trend(&conn, &[a, b], "ping_ms", 3);
        assert_eq!(tr.len(), 3);
        assert!(tr.iter().flatten().count() >= 1);
        // The registry, the state and the event are in the main store.
        let main = Store::open(&default_db(&root), Duration::from_secs(2)).unwrap();
        let (bot, closed, why): (String, Option<i64>, Option<String>) = main.conn.query_row("SELECT bot, closed_ms, close_reason FROM sockets WHERE id = ?1", [a], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((bot.as_str(), closed.is_some(), why.as_deref()), ("alpha", true, Some("replaced: slow")));
        let open: i64 = main.conn.query_row("SELECT COUNT(*) FROM sockets WHERE bot = 'alpha' AND closed_ms IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(open, 1);
        let (level, verdict, socket): (String, String, i64) = main.conn.query_row("SELECT level, verdict, socket_id FROM gateway_state WHERE bot = 'alpha'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((level.as_str(), verdict.as_str(), socket), ("warn", "stalled 130 s", b));
        let events: i64 = main.conn.query_row("SELECT COUNT(*) FROM events WHERE kind = 'gateway_connected'", [], |r| r.get(0)).unwrap();
        assert_eq!(events, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn finished_buckets_roll_up_minute_five_minute_hour_and_old_rows_are_dropped() {
        let root = temp_root("rollup");
        let conn = open_write(&db_path(&root)).unwrap();
        conn.execute("INSERT INTO series (socket_id, metric, unit, keep_raw_days) VALUES (7, 'ping_ms', 'ms', 30)", []).unwrap();
        // Three points in one minute, one in the next, at the start of an hour two hours ago.
        let hour = now_ms() / 3_600_000 - 2;
        let base = hour * 3_600_000;
        for (k, v) in [50.0, 60.0, 250.0].iter().enumerate() {
            conn.execute("INSERT INTO points VALUES (1, ?1, ?2)", params![base + k as i64 * 1000, v]).unwrap();
        }
        conn.execute("INSERT INTO points VALUES (1, ?1, 40.0)", [base + 61_000]).unwrap();
        // One point far in the past, beyond the keep.
        conn.execute("INSERT INTO points VALUES (1, ?1, 999.0)", [now_ms() - 40 * 86_400_000]).unwrap();
        roll_up(&conn, "rollup_1m", 60_000, "points", base / 60_000, base / 60_000 + 5).unwrap();
        roll_up(&conn, "rollup_5m", 300_000, "rollup_1m", base / 300_000, base / 300_000 + 1).unwrap();
        roll_up(&conn, "rollup_1h", 3_600_000, "rollup_5m", hour, hour + 1).unwrap();
        let row = |table: &str, bucket: i64| -> (i64, f64, f64, f64) {
            conn.query_row(&format!("SELECT n, sum, min, max FROM {table} WHERE series_id = 1 AND bucket_ms = ?1"), [bucket], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap()
        };
        assert_eq!(row("rollup_1m", base), (3, 360.0, 50.0, 250.0));
        assert_eq!(row("rollup_1m", base + 60_000), (1, 40.0, 40.0, 40.0));
        assert_eq!(row("rollup_5m", base), (4, 400.0, 40.0, 250.0));
        assert_eq!(row("rollup_1h", base), (4, 400.0, 40.0, 250.0));
        retire(&conn).unwrap();
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM points", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 4, "the point older than the keep is gone, the rest stay");
        let _ = std::fs::remove_dir_all(&root);
    }
}
