//! The experiment plan: which playbook a bot plays when it draws a target opponent.
//!
//! An experiment names its target opponents and two or more arms (playbook settings). When
//! any bot that takes part draws one of the targets, `assign` hands it the arm that has
//! played the fewest games in that exact cell (opponent, map, seat), so every arm meets
//! every opponent on every map from both seats equally often. Assignment and the match row
//! are written in one transaction, so two bots starting matches at the same moment cannot
//! both take the same slot.

use crate::store::{now_ms, MatchStart, Store};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

/// A match still marked in play after this long was abandoned (crash, kill, lost socket).
pub const PENDING_MS: i64 = 30 * 60 * 1000;

#[derive(Clone, Debug)]
pub struct Assignment {
    pub experiment_id: i64,
    pub experiment: String,
    pub arm: String,
    /// playbook settings of the arm, laid over the bot's own playbook for this opponent
    pub settings: Value,
}

impl Assignment {
    /// The playbook name written to logs and the store.
    pub fn playbook_name(&self) -> String {
        format!("{}:{}", self.experiment, self.arm)
    }
}

#[derive(Clone, Debug)]
pub struct Experiment {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub opponents: Vec<String>,
    pub bots: Option<Vec<String>>,
    pub games_per_arm: i64,
    pub priority: i64,
    pub note: Option<String>,
    /// arms still being handed out
    pub arms: Vec<(String, Value)>,
    /// arms stopped early, with the reason
    pub stopped: Vec<(String, String)>,
}

fn names(text: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(text).unwrap_or_default()
}

/// `*` matches any run of characters; everything else is literal.
pub fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, rest)) => {
            if !name.starts_with(head) {
                return false;
            }
            let tail = &name[head.len()..];
            (0..=tail.len()).filter(|&k| tail.is_char_boundary(k)).any(|k| name_matches(rest, &tail[k..]))
        }
    }
}

impl Store {
    pub fn experiments(&self, only_active: bool) -> rusqlite::Result<Vec<Experiment>> {
        let mut q = self.conn.prepare(
            "SELECT id, name, status, opponents, bots, games_per_arm, priority, note FROM experiments
             WHERE (?1 = 0 OR status = 'active') ORDER BY priority DESC, id",
        )?;
        let mut out: Vec<Experiment> = q
            .query_map([only_active as i64], |r| {
                Ok(Experiment {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    status: r.get(2)?,
                    opponents: names(&r.get::<_, String>(3)?),
                    bots: r.get::<_, Option<String>>(4)?.map(|t| names(&t)),
                    games_per_arm: r.get(5)?,
                    priority: r.get(6)?,
                    note: r.get(7)?,
                    arms: Vec::new(),
                    stopped: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut q = self.conn.prepare("SELECT name, playbook, stopped FROM arms WHERE experiment_id = ?1 ORDER BY rowid")?;
        for e in &mut out {
            let rows: Vec<(String, String, Option<String>)> = q.query_map([e.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            for (name, playbook, stopped) in rows {
                match stopped {
                    Some(why) => e.stopped.push((name, why)),
                    None => e.arms.push((name, serde_json::from_str(&playbook).unwrap_or(json!({})))),
                }
            }
        }
        Ok(out)
    }

    pub fn experiment(&self, name: &str) -> rusqlite::Result<Option<Experiment>> {
        Ok(self.experiments(false)?.into_iter().find(|e| e.name == name))
    }

    /// Adds an experiment. `arms` are (name, playbook settings); at least two.
    pub fn add_experiment(&self, name: &str, opponents: &[String], bots: Option<&[String]>, arms: &[(String, Value)], games_per_arm: i64, priority: i64, note: Option<&str>) -> rusqlite::Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO experiments (name, opponents, bots, games_per_arm, priority, created_ms, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![name, json!(opponents).to_string(), bots.map(|b| json!(b).to_string()), games_per_arm, priority, now_ms(), note],
        )?;
        let id = tx.last_insert_rowid();
        for (arm, playbook) in arms {
            tx.execute("INSERT INTO arms (experiment_id, name, playbook) VALUES (?1, ?2, ?3)", params![id, arm, playbook.to_string()])?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Adds an arm to an experiment that is already running. The new arm has no games yet,
    /// so it is handed out first in every cell until it has caught up with the others.
    pub fn add_arm(&self, experiment: &str, arm: &str, playbook: &Value) -> rusqlite::Result<usize> {
        self.conn.execute(
            "INSERT INTO arms (experiment_id, name, playbook) SELECT id, ?2, ?3 FROM experiments WHERE name = ?1",
            params![experiment, arm, playbook.to_string()],
        )
    }

    /// Stops handing out one arm; its games stay in the record.
    pub fn stop_arm(&self, experiment_id: i64, arm: &str, why: &str) -> rusqlite::Result<usize> {
        self.conn.execute("UPDATE arms SET stopped = ?3 WHERE experiment_id = ?1 AND name = ?2 AND stopped IS NULL", params![experiment_id, arm, why])
    }

    pub fn set_experiment_status(&self, name: &str, status: &str) -> rusqlite::Result<usize> {
        self.conn.execute("UPDATE experiments SET status = ?2 WHERE name = ?1", params![name, status])
    }

    /// Games an arm has finished (not voided), plus the ones in play right now.
    fn arm_games(&self, exp: i64, arm: &str, cell: Option<(&str, &str, Option<i64>)>, now: i64) -> rusqlite::Result<i64> {
        let live = "voided = 0 AND (result IS NOT NULL OR started_ms > ?3)";
        match cell {
            None => self.conn.query_row(
                &format!("SELECT COUNT(*) FROM matches WHERE experiment_id = ?1 AND arm = ?2 AND {live}"),
                params![exp, arm, now - PENDING_MS],
                |r| r.get(0),
            ),
            Some((opponent, map, seat)) => self.conn.query_row(
                &format!("SELECT COUNT(*) FROM matches WHERE experiment_id = ?1 AND arm = ?2 AND {live} AND opponent = ?4 AND map = ?5 AND seat IS ?6"),
                params![exp, arm, now - PENDING_MS, opponent, map, seat],
                |r| r.get(0),
            ),
        }
    }

    /// Called by a bot at `hello`. Picks the experiment arm for this match, if the opponent
    /// is a target and the bot takes part, and writes the match row either way.
    ///
    /// `experiments` is false for a bot that only reports results (the flagship).
    pub fn assign(&mut self, m: &MatchStart, base_playbook: &str, experiments: bool, ours: bool) -> rusqlite::Result<Option<Assignment>> {
        let now = now_ms();
        // A reconnect to a match we already started: give back what was assigned then.
        let known: Option<(Option<i64>, Option<String>)> = self
            .conn
            .query_row("SELECT experiment_id, arm FROM matches WHERE match_id = ?1 AND bot = ?2", params![m.match_id, m.bot], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((exp, arm)) = known {
            let (Some(exp), Some(arm)) = (exp, arm) else { return Ok(None) };
            // Straight from the table: the arm may have been stopped since the match began.
            let found: Option<(String, String)> = self
                .conn
                .query_row("SELECT e.name, a.playbook FROM arms a JOIN experiments e ON e.id = a.experiment_id WHERE a.experiment_id = ?1 AND a.name = ?2", params![exp, arm], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            return Ok(found.map(|(experiment, playbook)| Assignment { experiment_id: exp, experiment, arm, settings: serde_json::from_str(&playbook).unwrap_or(json!({})) }));
        }

        self.assign_locked(m, base_playbook, experiments && !ours, ours, now)
    }

    fn assign_locked(&mut self, m: &MatchStart, base_playbook: &str, experiments: bool, ours: bool, now: i64) -> rusqlite::Result<Option<Assignment>> {
        // IMMEDIATE takes the write lock before the counts are read, so the counts and the
        // insert are one step for every other process.
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<Option<Assignment>> {
            let mut chosen: Option<Assignment> = None;
            if experiments {
                for e in self.experiments(true)? {
                    if e.arms.len() < 2 || !e.opponents.iter().any(|p| name_matches(p, &m.opponent)) {
                        continue;
                    }
                    if let Some(bots) = &e.bots {
                        if !bots.iter().any(|b| *b == m.bot) {
                            continue;
                        }
                    }
                    // Fewest games in this cell, then against this opponent, then overall.
                    let mut best: Option<((i64, i64, i64), usize)> = None;
                    let mut open = false;
                    for (k, (arm, _)) in e.arms.iter().enumerate() {
                        let total = self.arm_games(e.id, arm, None, now)?;
                        if total >= e.games_per_arm {
                            continue;
                        }
                        open = true;
                        let cell = self.arm_games(e.id, arm, Some((&m.opponent, &m.map, m.seat)), now)?;
                        let versus: i64 = self.conn.query_row(
                            "SELECT COUNT(*) FROM matches WHERE experiment_id = ?1 AND arm = ?2 AND opponent = ?3 AND voided = 0 AND (result IS NOT NULL OR started_ms > ?4)",
                            params![e.id, arm, m.opponent, now - PENDING_MS],
                            |r| r.get(0),
                        )?;
                        let key = (cell, versus, total);
                        if best.map(|(b, _)| key < b).unwrap_or(true) {
                            best = Some((key, k));
                        }
                    }
                    if !open {
                        // Every arm has its games: the experiment is complete.
                        self.conn.execute("UPDATE experiments SET status = 'done' WHERE id = ?1", [e.id])?;
                        continue;
                    }
                    if let Some((_, k)) = best {
                        let (arm, settings) = e.arms[k].clone();
                        chosen = Some(Assignment { experiment_id: e.id, experiment: e.name.clone(), arm, settings });
                        break;
                    }
                }
            }
            let name = chosen.as_ref().map(|a| a.playbook_name()).unwrap_or_else(|| base_playbook.to_string());
            self.match_started(m, &name, chosen.as_ref().map(|a| (a.experiment_id, a.arm.as_str())), ours, "live", now)?;
            Ok(chosen)
        })();
        match &result {
            Ok(_) => self.conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_store(name: &str) -> Store {
        let path = std::env::temp_dir().join(format!("fleet-test-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Store::open(&path, Duration::from_secs(5)).unwrap()
    }

    fn start(id: usize, opponent: &str, map: &str, seat: i64) -> MatchStart {
        MatchStart { match_id: format!("m{id}"), bot: "scout".into(), version: "v1".into(), release: None, opponent: opponent.into(), map: map.into(), seat: Some(seat), seed: None }
    }

    #[test]
    fn patterns() {
        assert!(name_matches("Shelbot*", "Shelbot4"));
        assert!(name_matches("*", "anyone"));
        assert!(name_matches("sir-*-v2", "sir-lancelot-v2"));
        assert!(!name_matches("Shelbot*", "shelbot4"));
        assert!(!name_matches("Stuart", "StuartSutcliffe"));
    }

    #[test]
    fn arms_are_balanced_in_every_cell() {
        let mut s = temp_store("balance");
        let arms = vec![("a".to_string(), json!({})), ("b".to_string(), json!({ "x": 1 })), ("c".to_string(), json!({ "x": 2 }))];
        s.add_experiment("e", &["top*".to_string()], None, &arms, 1000, 0, None).unwrap();
        let mut id = 0;
        let mut seen = std::collections::HashMap::new();
        for _round in 0..6 {
            for opponent in ["top1", "top2", "other"] {
                for map in ["m1", "m2"] {
                    for seat in [1, 2] {
                        id += 1;
                        let got = s.assign(&start(id, opponent, map, seat), "base", true, false).unwrap();
                        if opponent == "other" {
                            assert!(got.is_none());
                        } else {
                            *seen.entry((opponent, map, seat, got.unwrap().arm)).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
        // 6 rounds over 3 arms: exactly 2 games per arm in each of the 8 cells.
        assert_eq!(seen.len(), 24);
        assert!(seen.values().all(|n| *n == 2), "{seen:?}");
    }

    #[test]
    fn stops_at_the_quota_and_skips_our_own_bots() {
        let mut s = temp_store("quota");
        let arms = vec![("a".to_string(), json!({})), ("b".to_string(), json!({ "x": 1 }))];
        s.add_experiment("e", &["rival".to_string()], Some(&["scout".to_string()]), &arms, 2, 0, None).unwrap();
        // Our own bot as the opponent, and a bot that does not take part, get nothing.
        assert!(s.assign(&start(100, "rival", "m", 1), "base", true, true).unwrap().is_none());
        assert!(s.assign(&start(101, "rival", "m", 1), "base", false, false).unwrap().is_none());
        for id in 0..4 {
            assert!(s.assign(&start(id, "rival", "m", 1), "base", true, false).unwrap().is_some());
        }
        // Both arms have their two games: the fifth match plays the bot's own playbook.
        assert!(s.assign(&start(4, "rival", "m", 1), "base", true, false).unwrap().is_none());
        assert_eq!(s.experiment("e").unwrap().unwrap().status, "done");
        // A reconnect to an earlier match gets the arm it was given then.
        let again = s.assign(&start(0, "rival", "m", 1), "base", true, false).unwrap().unwrap();
        assert_eq!(again.arm, "a");
    }
}
