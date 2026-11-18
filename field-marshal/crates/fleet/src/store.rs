//! The shared results store: one SQLite file that every bot process, the supervisor and the
//! release tools read and write. WAL mode, so readers never block the bots.
//!
//! A bot writes one row per match: at `hello` (who, where, which playbook) and again at `end`
//! (the result). Everything else here is bookkeeping around that table.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The repository root: `VICTRIS_ROOT`, else the first directory above the executable or
/// the working directory that holds both `bots/` and `field-marshal/`.
pub fn repo_root() -> PathBuf {
    if let Ok(r) = std::env::var("VICTRIS_ROOT") {
        return PathBuf::from(r);
    }
    let starts = [std::env::current_exe().ok(), std::env::current_dir().ok()];
    for start in starts.into_iter().flatten() {
        let mut p = start.as_path();
        loop {
            if p.join("field-marshal").is_dir() && p.join("fleet").is_dir() {
                return p.to_path_buf();
            }
            match p.parent() {
                Some(up) => p = up,
                None => break,
            }
        }
    }
    PathBuf::from(".")
}

pub fn data_dir(root: &Path) -> PathBuf {
    root.join("data")
}

pub fn default_db(root: &Path) -> PathBuf {
    data_dir(root).join("fleet.db")
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS matches (
  match_id TEXT NOT NULL,
  bot TEXT NOT NULL,
  version TEXT,
  release TEXT,
  opponent TEXT,
  opponent_version TEXT,
  opponent_ours INTEGER NOT NULL DEFAULT 0,
  map TEXT,
  seat INTEGER,
  seed INTEGER,
  playbook TEXT,
  experiment_id INTEGER,
  arm TEXT,
  started_ms INTEGER,
  ended_ms INTEGER,
  result TEXT,
  reason TEXT,
  final_tick INTEGER,
  rating_change REAL,
  voided INTEGER NOT NULL DEFAULT 0,
  replay_url TEXT,
  think_p50_us INTEGER,
  think_p99_us INTEGER,
  think_max_us INTEGER,
  mismatches INTEGER,
  panics INTEGER,
  refusals INTEGER,
  min_budget_ms INTEGER,
  detail TEXT,
  source TEXT NOT NULL DEFAULT 'live',
  PRIMARY KEY (match_id, bot)
);
CREATE INDEX IF NOT EXISTS matches_opponent ON matches (opponent, bot);
CREATE INDEX IF NOT EXISTS matches_arm ON matches (experiment_id, arm, opponent, map, seat);
CREATE INDEX IF NOT EXISTS matches_release ON matches (release, bot);

CREATE TABLE IF NOT EXISTS experiments (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  status TEXT NOT NULL DEFAULT 'active',
  opponents TEXT NOT NULL,
  bots TEXT,
  games_per_arm INTEGER NOT NULL DEFAULT 100,
  priority INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL,
  note TEXT
);
CREATE TABLE IF NOT EXISTS arms (
  experiment_id INTEGER NOT NULL,
  name TEXT NOT NULL,
  playbook TEXT NOT NULL,
  PRIMARY KEY (experiment_id, name)
);

CREATE TABLE IF NOT EXISTS releases (
  id TEXT PRIMARY KEY,
  seq INTEGER NOT NULL,
  git_sha TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  status TEXT NOT NULL,
  path TEXT NOT NULL,
  gate TEXT,
  evidence TEXT,
  note TEXT
);
CREATE TABLE IF NOT EXISTS deployments (
  id INTEGER PRIMARY KEY,
  bot TEXT NOT NULL,
  release TEXT NOT NULL,
  version TEXT NOT NULL,
  ts_ms INTEGER NOT NULL,
  reason TEXT
);
CREATE TABLE IF NOT EXISTS bot_state (
  bot TEXT PRIMARY KEY,
  release TEXT,
  version TEXT,
  halted TEXT,
  updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY,
  bot TEXT NOT NULL,
  release TEXT,
  version TEXT,
  pid INTEGER,
  started_ms INTEGER NOT NULL,
  ended_ms INTEGER,
  exit TEXT
);
CREATE TABLE IF NOT EXISTS replays (
  match_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  priority INTEGER NOT NULL DEFAULT 3,
  ended TEXT,
  map TEXT,
  reason TEXT,
  final_tick INTEGER,
  winner INTEGER,
  bot1 TEXT,
  version1 TEXT,
  rating1 REAL,
  bot2 TEXT,
  version2 TEXT,
  rating2 REAL,
  ours INTEGER NOT NULL DEFAULT 0,
  added_ms INTEGER NOT NULL,
  tries INTEGER NOT NULL DEFAULT 0,
  next_try_ms INTEGER NOT NULL DEFAULT 0,
  fetched_ms INTEGER,
  bytes INTEGER,
  ticks INTEGER,
  note TEXT
);
CREATE INDEX IF NOT EXISTS replays_queue ON replays (status, priority, ended);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY,
  ts_ms INTEGER NOT NULL,
  bot TEXT,
  kind TEXT NOT NULL,
  detail TEXT
);
";

pub struct Store {
    pub conn: Connection,
}

/// What a bot knows about a match when it starts.
#[derive(Clone, Debug, Default)]
pub struct MatchStart {
    pub match_id: String,
    pub bot: String,
    pub version: String,
    pub release: Option<String>,
    pub opponent: String,
    pub map: String,
    pub seat: Option<i64>,
    pub seed: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct BotState {
    pub bot: String,
    pub release: Option<String>,
    pub version: Option<String>,
    pub halted: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub id: String,
    pub seq: i64,
    pub git_sha: String,
    pub created_ms: i64,
    pub status: String,
    pub path: String,
    pub gate: Option<Value>,
    pub evidence: Option<Value>,
    pub note: Option<String>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}
fn i(v: &Value, k: &str) -> Option<i64> {
    v.get(k).and_then(Value::as_i64)
}

impl Store {
    /// Opens (and creates) the store. `busy` bounds how long a writer waits for another one.
    pub fn open(path: &Path, busy: Duration) -> rusqlite::Result<Store> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(busy)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    pub fn open_default() -> rusqlite::Result<Store> {
        Store::open(&default_db(&repo_root()), Duration::from_secs(5))
    }

    pub fn event(&self, bot: Option<&str>, kind: &str, detail: &Value) {
        let _ = self.conn.execute(
            "INSERT INTO events (ts_ms, bot, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now_ms(), bot, kind, detail.to_string()],
        );
    }

    // ---- matches ---------------------------------------------------------------------------

    /// Records the start of a match. A reconnect to the same match keeps the first row.
    pub fn match_started(&self, m: &MatchStart, playbook: &str, experiment: Option<(i64, &str)>, ours: bool, source: &str, ts: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO matches (match_id, bot, version, release, opponent, opponent_ours, map, seat, seed, playbook, experiment_id, arm, started_ms, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![m.match_id, m.bot, m.version, m.release, m.opponent, ours as i64, m.map, m.seat, m.seed, playbook,
                experiment.map(|e| e.0), experiment.map(|e| e.1), ts, source],
        )?;
        Ok(())
    }

    /// Records the end of a match from the bot's `match` log line (see `session.rs`).
    pub fn match_finished(&self, bot: &str, line: &Value, ts: i64) -> rusqlite::Result<usize> {
        let Some(id) = s(line, "match") else { return Ok(0) };
        let think = line.get("think_us").cloned().unwrap_or(Value::Null);
        let detail = serde_json::json!({ "damage": line.get("damage"), "tracking": line.get("tracking"), "sims": line.get("sims") });
        self.conn.execute(
            "UPDATE matches SET ended_ms = ?3, result = ?4, reason = ?5, final_tick = ?6, rating_change = ?7, voided = ?8,
               replay_url = ?9, opponent_version = ?10, think_p50_us = ?11, think_p99_us = ?12, think_max_us = ?13,
               mismatches = ?14, panics = ?15, refusals = ?16, min_budget_ms = ?17, detail = ?18
             WHERE match_id = ?1 AND bot = ?2",
            params![
                id,
                bot,
                ts,
                s(line, "result"),
                s(line, "reason"),
                i(line, "final_tick"),
                line.get("rating_change").and_then(Value::as_f64),
                line.get("voided").map(|v| v.as_bool().unwrap_or(!v.is_null())).unwrap_or(false) as i64,
                s(line, "replay_url"),
                s(line, "opponent_version"),
                i(&think, "p50"),
                i(&think, "p99"),
                i(&think, "max"),
                line.get("tracking").and_then(|t| i(t, "mismatch")),
                i(line, "panics"),
                i(line, "server_refusals"),
                i(line, "min_budget_ms"),
                detail.to_string(),
            ],
        )
    }

    // ---- bots ------------------------------------------------------------------------------

    pub fn bot_state(&self, bot: &str) -> rusqlite::Result<Option<BotState>> {
        self.conn
            .query_row("SELECT bot, release, version, halted FROM bot_state WHERE bot = ?1", [bot], |r| {
                Ok(BotState { bot: r.get(0)?, release: r.get(1)?, version: r.get(2)?, halted: r.get(3)? })
            })
            .optional()
    }

    pub fn set_deployment(&self, bot: &str, release: &str, version: &str, reason: &str) -> rusqlite::Result<()> {
        let ts = now_ms();
        self.conn.execute(
            "INSERT INTO bot_state (bot, release, version, halted, updated_ms) VALUES (?1, ?2, ?3, NULL, ?4)
             ON CONFLICT(bot) DO UPDATE SET release = ?2, version = ?3, updated_ms = ?4",
            params![bot, release, version, ts],
        )?;
        self.conn.execute("INSERT INTO deployments (bot, release, version, ts_ms, reason) VALUES (?1, ?2, ?3, ?4, ?5)", params![bot, release, version, ts, reason])?;
        Ok(())
    }

    pub fn set_halted(&self, bot: &str, why: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO bot_state (bot, halted, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(bot) DO UPDATE SET halted = ?2, updated_ms = ?3",
            params![bot, why, now_ms()],
        )?;
        Ok(())
    }

    /// The next free ladder version label for a bot: one above the highest `v<N>` it has used.
    pub fn next_version(&self, bot: &str) -> rusqlite::Result<String> {
        let mut top = 0i64;
        for sql in ["SELECT DISTINCT version FROM matches WHERE bot = ?1", "SELECT DISTINCT version FROM deployments WHERE bot = ?1"] {
            let mut q = self.conn.prepare(sql)?;
            for v in q.query_map([bot], |r| r.get::<_, Option<String>>(0))? {
                if let Some(n) = v?.as_deref().and_then(|v| v.strip_prefix('v')).and_then(|n| n.parse::<i64>().ok()) {
                    top = top.max(n);
                }
            }
        }
        Ok(format!("v{}", top + 1))
    }

    // ---- releases --------------------------------------------------------------------------

    fn release_row(r: &rusqlite::Row) -> rusqlite::Result<Release> {
        let json = |t: Option<String>| t.and_then(|t| serde_json::from_str(&t).ok());
        Ok(Release {
            id: r.get(0)?,
            seq: r.get(1)?,
            git_sha: r.get(2)?,
            created_ms: r.get(3)?,
            status: r.get(4)?,
            path: r.get(5)?,
            gate: json(r.get(6)?),
            evidence: json(r.get(7)?),
            note: r.get(8)?,
        })
    }

    const RELEASE_COLS: &'static str = "id, seq, git_sha, created_ms, status, path, gate, evidence, note";

    pub fn release(&self, id: &str) -> rusqlite::Result<Option<Release>> {
        self.conn.query_row(&format!("SELECT {} FROM releases WHERE id = ?1", Self::RELEASE_COLS), [id], Self::release_row).optional()
    }

    pub fn releases(&self) -> rusqlite::Result<Vec<Release>> {
        let mut q = self.conn.prepare(&format!("SELECT {} FROM releases ORDER BY seq", Self::RELEASE_COLS))?;
        let rows = q.query_map([], Self::release_row)?.collect();
        rows
    }

    /// The release the flagship runs: the newest one marked `live`.
    pub fn live_release(&self) -> rusqlite::Result<Option<Release>> {
        self.conn
            .query_row(&format!("SELECT {} FROM releases WHERE status = 'live' ORDER BY seq DESC LIMIT 1", Self::RELEASE_COLS), [], Self::release_row)
            .optional()
    }

    pub fn next_release_seq(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM releases", [], |r| r.get(0))
    }

    pub fn add_release(&self, id: &str, seq: i64, sha: &str, path: &str, note: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO releases (id, seq, git_sha, created_ms, status, path, note) VALUES (?1, ?2, ?3, ?4, 'built', ?5, ?6)",
            params![id, seq, sha, now_ms(), path, note],
        )?;
        Ok(())
    }

    pub fn set_release_status(&self, id: &str, status: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE releases SET status = ?2 WHERE id = ?1", params![id, status])?;
        Ok(())
    }

    pub fn set_release_json(&self, id: &str, column: &str, v: &Value) -> rusqlite::Result<()> {
        assert!(column == "gate" || column == "evidence");
        self.conn.execute(&format!("UPDATE releases SET {column} = ?2 WHERE id = ?1"), params![id, v.to_string()])?;
        Ok(())
    }
}
