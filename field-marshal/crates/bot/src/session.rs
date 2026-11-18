//! One bot's conversation with the ladder, independent of how the frames travel.
//!
//! `Session::handle` takes one server frame and returns what to send back. The WebSocket
//! client and the stdio test transport both drive it, so the message path tested locally
//! is the one used on the ladder.

use crate::policy::{decide, Playbook};
use crate::tracker::{Sync, Tracker};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use victris_engine::geom::DIR_NAMES;
use victris_engine::hash::canonical_json;
use victris_engine::observe::View;
use victris_engine::turn::{AttackIn, DirIn, RawTurn, TargetIn};

/// Errors that reconnecting cannot fix: the bot stops.
pub const FATAL_CODES: [&str; 15] = [
    "missing_authorization",
    "invalid_token",
    "suspended",
    "banned",
    "retired",
    "account_banned",
    "account_required",
    "human_only",
    "replaced",
    "unknown_ladder",
    "ladder_closed",
    "enrollment_required",
    "wrong_ladder",
    "membership_revoked",
    "ladder_unavailable",
];

/// Which playbook to use against whom. The default plays everyone not named.
#[derive(Clone, Debug, Default)]
pub struct Playbooks {
    pub default: Playbook,
    pub by_opponent: HashMap<String, Playbook>,
}

impl Playbooks {
    /// `{"default": {...}, "opponents": {"<bot name>": {"name": "...", ...}}}`; every field optional.
    pub fn from_json(v: &Value) -> Playbooks {
        let mut p = Playbooks::default();
        if let Some(d) = v.get("default") {
            p.default = Playbook::from_json(d.get("name").and_then(Value::as_str).unwrap_or("matrix-2"), d);
        }
        if let Some(o) = v.get("opponents").and_then(Value::as_object) {
            for (opponent, settings) in o {
                let name = settings.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("vs-{opponent}"));
                p.by_opponent.insert(opponent.clone(), Playbook::from_json(&name, settings));
            }
        }
        p
    }
    pub fn pick(&self, opponent: &str) -> &Playbook {
        self.by_opponent.get(opponent).unwrap_or(&self.default)
    }
}

pub struct Config {
    pub bot: String,
    pub version: String,
    pub ladder_id: Option<String>,
    pub playbooks: Playbooks,
    pub log_dir: Option<PathBuf>,
    /// stop after this many matches
    pub max_matches: Option<u32>,
}

struct Match {
    id: String,
    opponent: String,
    map: String,
    playbook: Playbook,
    /// `None` when the match's rules or map could not be loaded: we play idle turns
    tracker: Option<Tracker>,
    last_turn: Option<(i32, RawTurn)>,
    think_us: Vec<u32>,
    sims: u64,
    rejected: u32,
    server_refusals: u32,
    panics: u32,
    /// ticks where the test harness supplied the true state, and how many the belief matched
    truth_checked: u32,
    truth_exact: u32,
    truth_diffs: HashMap<String, u32>,
    min_budget_ms: i64,
    /// damage by source ("taken:ranged", "dealt:wall_slam", ...) and pickups, from events
    damage: std::collections::BTreeMap<String, i64>,
}

pub enum Out {
    Send(String),
    /// the configured number of matches is done, or a stop was requested
    Done,
    Fatal(String),
}

pub struct Session {
    pub cfg: Config,
    cur: Option<Match>,
    pub played: u32,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
    /// set from outside to stop after the current match
    pub stop_after_match: bool,
    log: Option<std::fs::File>,
    log_day: String,
}

fn utc_day() -> String {
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn turn_json(tick: i32, t: &RawTurn, e: &victris_engine::Engine) -> Value {
    let mut o = json!({ "tick": tick });
    if let DirIn::Dir(d) = t.mv {
        o["move"] = json!(DIR_NAMES[d as usize]);
    }
    if let DirIn::Dir(d) = t.dash {
        o["dash"] = json!(DIR_NAMES[d as usize]);
    }
    if let AttackIn::Named { attack, target } = t.attack {
        let name = &e.rules.attacks[attack as usize].name;
        o["attack"] = match target {
            TargetIn::Tile(p) => json!({ "name": name, "target": p }),
            _ => json!({ "name": name }),
        };
    }
    o
}

/// Paths where two JSON values differ, for the truth check's report.
fn diff_paths(a: &Value, b: &Value, path: &str, out: &mut Vec<String>) {
    if out.len() >= 8 || a == b {
        return;
    }
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => diff_paths(v, w, &format!("{path}.{k}"), out),
                    None => out.push(format!("{path}.{k}")),
                }
            }
            for k in y.keys() {
                if !x.contains_key(k) {
                    out.push(format!("{path}.{k}"));
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (v, w)) in x.iter().zip(y).enumerate() {
                diff_paths(v, w, &format!("{path}[{i}]"), out);
            }
        }
        _ => out.push(path.to_string()),
    }
}

impl Session {
    pub fn new(cfg: Config) -> Session {
        Session { cfg, cur: None, played: 0, wins: 0, losses: 0, draws: 0, stop_after_match: false, log: None, log_day: String::new() }
    }

    pub fn queue_frame(&self) -> String {
        let mut q = json!({ "type": "queue", "version": self.cfg.version });
        if let Some(l) = &self.cfg.ladder_id {
            q["ladder_id"] = json!(l);
        }
        q.to_string()
    }

    pub fn in_match(&self) -> bool {
        self.cur.is_some()
    }

    fn log(&mut self, mut line: Value) {
        let Some(dir) = self.cfg.log_dir.clone() else { return };
        let day = utc_day();
        if self.log.is_none() || self.log_day != day {
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("{}-{day}.jsonl", self.cfg.bot));
            self.log = std::fs::OpenOptions::new().create(true).append(true).open(path).ok();
            self.log_day = day;
        }
        line["ts"] = json!(now_ms());
        line["bot"] = json!(self.cfg.bot);
        line["version"] = json!(self.cfg.version);
        if let Some(f) = self.log.as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }

    pub fn note(&mut self, kind: &str, detail: Value) {
        eprintln!("[{}] {kind}: {detail}", self.cfg.bot);
        self.log(json!({ "t": kind, "detail": detail }));
    }

    /// Handles one frame from the server.
    pub fn handle(&mut self, text: &str) -> Vec<Out> {
        let received = Instant::now();
        let msg: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                self.note("bad_frame", json!(e.to_string()));
                return vec![];
            }
        };
        match msg.get("type").and_then(Value::as_str) {
            Some("hello") => self.on_hello(&msg),
            Some("observation") => self.on_observation(&msg, received),
            Some("end") => self.on_end(&msg),
            Some("error") => {
                let code = msg.get("code").and_then(Value::as_str).unwrap_or("?").to_string();
                self.note("server_error", json!({ "code": code, "message": msg.get("message"), "retry_after_s": msg.get("retry_after_s") }));
                if FATAL_CODES.contains(&code.as_str()) {
                    vec![Out::Fatal(code)]
                } else {
                    vec![]
                }
            }
            Some("server_draining") => {
                self.note("server_draining", json!(msg.get("drain_deadline")));
                vec![]
            }
            Some("queued") => vec![],
            other => {
                self.note("unknown_message", json!(other));
                vec![]
            }
        }
    }

    fn on_hello(&mut self, msg: &Value) -> Vec<Out> {
        let id = msg.get("match_id").and_then(Value::as_str).unwrap_or("?").to_string();
        // The same match again means a reconnect: keep what we know.
        if self.cur.as_ref().map(|m| m.id != id).unwrap_or(true) {
            let opponent = msg.get("opponent").and_then(|o| o.get("name")).and_then(Value::as_str).unwrap_or("?").to_string();
            let playbook = self.cfg.playbooks.pick(&opponent).clone();
            let tracker = match Tracker::new(msg) {
                Ok(t) => Some(t),
                Err(e) => {
                    // Rules or a map this build cannot load: play safe (idle) and say so loudly.
                    self.note("safe_mode", json!({ "match": id, "why": e }));
                    None
                }
            };
            let map = msg.get("map_name").and_then(Value::as_str).unwrap_or("?").to_string();
            self.log(json!({
                "t": "hello", "match": id, "opponent": opponent, "playbook": playbook.name, "map": map,
                "seat": msg.get("bot_id"), "seed": msg.get("seed_public"),
                "rules_version": msg.get("rules").and_then(|r| r.get("version")),
                "rules_hash": tracker.as_ref().map(|t| t.engine.rules.hash.clone()),
            }));
            self.cur = Some(Match {
                id,
                opponent,
                map,
                playbook,
                tracker,
                last_turn: None,
                think_us: Vec::new(),
                sims: 0,
                rejected: 0,
                server_refusals: 0,
                panics: 0,
                truth_checked: 0,
                truth_exact: 0,
                truth_diffs: HashMap::new(),
                min_budget_ms: i64::MAX,
                damage: Default::default(),
            });
        }
        vec![Out::Send(json!({ "type": "ready" }).to_string())]
    }

    fn on_observation(&mut self, msg: &Value, received: Instant) -> Vec<Out> {
        let tick = msg.get("tick").and_then(Value::as_i64).unwrap_or(0) as i32;
        // The match is taken out while we work on it, so logging can borrow the session.
        let Some(mut m) = self.cur.take() else { return vec![Out::Send(json!({ "tick": tick }).to_string())] };
        let out = self.observe_in(&mut m, msg, tick, received);
        self.cur = Some(m);
        out
    }

    fn observe_in(&mut self, m: &mut Match, msg: &Value, tick: i32, received: Instant) -> Vec<Out> {
        let idle = json!({ "tick": tick }).to_string();
        let events: &[Value] = msg.get("events").and_then(Value::as_array).map(|a| a.as_slice()).unwrap_or(&[]);
        let my_id = msg.get("self").and_then(|s| s.get("id")).and_then(Value::as_i64);
        for e in events {
            // Damage by source, both ways, for the match line.
            let kind = e.get("kind").and_then(Value::as_str).unwrap_or("");
            let dmg = e.get("damage").and_then(Value::as_i64).unwrap_or(0);
            let source = match kind {
                "hit" => e.get("attack").and_then(Value::as_str).unwrap_or("?"),
                "wall_slam" => "wall_slam",
                _ => "",
            };
            if !source.is_empty() {
                let side = if e.get("target").and_then(Value::as_i64) == my_id { "taken" } else { "dealt" };
                *m.damage.entry(format!("{side}:{source}")).or_default() += dmg;
            }
            if kind == "zone_damage" && e.get("fighter").and_then(Value::as_i64) == my_id {
                *m.damage.entry("taken:zone".into()).or_default() += dmg;
            }
            if kind == "pickup" {
                let side = if e.get("fighter").and_then(Value::as_i64) == my_id { "mine" } else { "theirs" };
                *m.damage.entry(format!("pickup:{side}:{}", e.get("pickup").and_then(Value::as_str).unwrap_or("?"))).or_default() += 1;
            }
            if e.get("kind").and_then(Value::as_str) == Some("rejected") && e.get("fighter").and_then(Value::as_i64) == my_id {
                if e.get("part").and_then(Value::as_str) == Some("(message)") {
                    m.server_refusals += 1;
                } else {
                    m.rejected += 1;
                }
            }
        }
        if let Some(b) = msg.get("self").and_then(|s| s.get("budget_ms_left")).and_then(Value::as_i64) {
            m.min_budget_ms = m.min_budget_ms.min(b);
        }
        let Some(tracker) = m.tracker.as_mut() else { return vec![Out::Send(idle)] };

        let view = match View::from_json(&tracker.engine, msg) {
            Ok(v) => v,
            Err(e) => {
                self.note("bad_observation", json!(e));
                return vec![Out::Send(idle)];
            }
        };
        let prev = m.last_turn.filter(|(t, _)| *t + 1 == tick).map(|(_, turn)| turn);
        // A bug in the tracker or the policy must cost one idle tick, never the match.
        let playbook = m.playbook.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let sync = tracker.observe(&view, events, prev);
            let d = decide(tracker, &view, &playbook);
            (sync, d)
        }));
        let (sync, decision) = match result {
            Ok(x) => x,
            Err(_) => {
                m.panics += 1;
                tracker.belief = None; // start again from the next observation
                m.last_turn = Some((tick, RawTurn::IDLE));
                self.note("panic", json!({ "tick": tick }));
                return vec![Out::Send(idle)];
            }
        };

        // Test harness only: the true state rides along and the belief is compared with it.
        let mut truth_note = Value::Null;
        if let (Some(truth), Some(belief)) = (msg.get("_truth"), tracker.belief.as_ref()) {
            m.truth_checked += 1;
            let mut ours = String::new();
            tracker.engine.state_json(belief, &mut ours);
            // `state_end` of an idle fighter is a leftover the rules never read again.
            let mut truth = truth.clone();
            let mut ours_v: Value = serde_json::from_str(&ours).unwrap_or(Value::Null);
            for v in [&mut truth, &mut ours_v] {
                if let Some(fs) = v.get_mut("fighters").and_then(Value::as_array_mut) {
                    for f in fs {
                        if f.get("state").and_then(Value::as_str) == Some("idle") {
                            f["state_end"] = json!(0);
                        }
                    }
                }
            }
            let truth = &truth;
            ours.clear();
            canonical_json(&ours_v, &mut ours);
            let mut theirs = String::new();
            canonical_json(truth, &mut theirs);
            if ours == theirs {
                m.truth_exact += 1;
            } else {
                let mut paths = Vec::new();
                diff_paths(&ours_v, truth, "", &mut paths);
                // Name the fighter by role so counts add up across seats.
                let me = tracker.me;
                let label: Vec<String> = paths
                    .iter()
                    .map(|p| p.replace(&format!(".fighters[{me}]"), ".me").replace(&format!(".fighters[{}]", 1 - me), ".enemy"))
                    .map(|p| p.split('[').next().unwrap_or("").to_string())
                    .collect();
                let mut key: Vec<String> = label;
                key.sort();
                key.dedup();
                let key = key.join(" ");
                *m.truth_diffs.entry(format!("{}|{}", if tracker.enemy_visible { "seen" } else { "unseen" }, key)).or_default() += 1;
                truth_note = json!(paths);
            }
        }

        let turn = turn_json(tick, &decision.turn, &tracker.engine);
        m.last_turn = Some((tick, decision.turn));
        let us = received.elapsed().as_micros() as u32;
        m.think_us.push(us);
        m.sims += decision.sims as u64;
        let line = json!({
            "t": "tick", "match": m.id, "tick": tick, "us": us, "sync": sync.name(), "sims": decision.sims,
            "why": decision.note, "score": (decision.score * 100.0).round() / 100.0, "turn": turn,
            "hp": [view.me.hp, view.enemy.as_ref().map(|e| e.hp)], "enemy_seen": view.enemy.is_some(),
            "budget_ms": msg.get("self").and_then(|s| s.get("budget_ms_left")),
            "truth_diff": truth_note,
        });
        if sync == Sync::Mismatch {
            let why: String = tracker.last_mismatch.take().unwrap_or_default().chars().take(1500).collect();
            let detail = json!({ "match": m.id, "tick": tick, "why": why, "events": events });
            self.note("prediction_mismatch", detail);
        }
        self.log(line);
        vec![Out::Send(turn.to_string())]
    }

    fn on_end(&mut self, msg: &Value) -> Vec<Out> {
        self.played += 1;
        let result = msg.get("result").and_then(Value::as_str).unwrap_or("?").to_string();
        match result.as_str() {
            "win" => self.wins += 1,
            "loss" => self.losses += 1,
            _ => self.draws += 1,
        }
        if let Some(mut m) = self.cur.take() {
            m.think_us.sort_unstable();
            let pct = |p: f64| m.think_us.get(((m.think_us.len() as f64 - 1.0) * p).max(0.0) as usize).copied().unwrap_or(0);
            let mut diffs: Vec<(&String, &u32)> = m.truth_diffs.iter().collect();
            diffs.sort_by(|a, b| b.1.cmp(a.1));
            let line = json!({
                "t": "match", "match": m.id, "opponent": m.opponent, "opponent_version": msg.get("opponent_version"),
                "playbook": m.playbook.name, "map": m.map, "result": result, "reason": msg.get("reason"),
                "final_tick": msg.get("final_tick"), "rating_change": msg.get("rating_change"), "voided": msg.get("voided"),
                "replay_url": msg.get("replay_url"),
                "think_us": { "p50": pct(0.5), "p99": pct(0.99), "max": m.think_us.last() },
                "sims": m.sims, "rejected_parts": m.rejected, "server_refusals": m.server_refusals, "panics": m.panics,
                "min_budget_ms": if m.min_budget_ms == i64::MAX { Value::Null } else { json!(m.min_budget_ms) },
                "tracking": m.tracker.as_ref().map(|t| t.stats.to_json()),
                "damage": m.damage,
                "truth": if m.truth_checked > 0 {
                    json!({ "checked": m.truth_checked, "exact": m.truth_exact,
                            "diffs": diffs.iter().take(12).map(|(k, n)| json!([k, n])).collect::<Vec<_>>() })
                } else { Value::Null },
            });
            eprintln!(
                "[{}] {} vs {} on {} ({}, tick {}) playbook {}",
                self.cfg.bot,
                result,
                m.opponent,
                m.map,
                msg.get("reason").and_then(Value::as_str).unwrap_or("?"),
                msg.get("final_tick").and_then(Value::as_i64).unwrap_or(-1),
                m.playbook.name
            );
            self.log(line);
        }
        if self.stop_after_match || self.cfg.max_matches.map(|n| self.played >= n).unwrap_or(false) {
            vec![Out::Done]
        } else {
            vec![Out::Send(self.queue_frame())]
        }
    }
}
