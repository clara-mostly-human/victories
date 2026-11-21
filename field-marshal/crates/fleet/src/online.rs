//! The offline watchdog and the load guard. The watchdog asks the ladder, once a minute, whether each bot that should be
//! up is online, and raises an alert when one has been offline for two minutes.
//!
//! It only reads the ladder's public API. It never starts, stops or connects as any bot:
//! bringing a bot back is its owner's job. An alert is one loud line in
//! `logs/fleet-alerts.log`, an event in the store and a notification on the Mac; it is
//! repeated every 15 minutes while the bot stays down, and a recovery line follows when it
//! is back.

use crate::config::Config;
use crate::store::{default_db, Store};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const CHECK_EVERY: Duration = Duration::from_secs(60);
pub const OFFLINE_AFTER: Duration = Duration::from_secs(120);
pub const REPEAT_EVERY: Duration = Duration::from_secs(900);

/// UTC time as `2026-11-18T03:04:05Z`.
pub fn utc_stamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Is the bot on the ladder? Judged on this machine, from its own log: a bot that has
/// written a turn, a match or a connection event lately is on. Nothing here asks the
/// ladder: the gateway is the only thing on this machine that talks to it.
pub fn is_online(root: &Path, bot: &str) -> Result<bool, String> {
    Ok(wrote_recently(root, bot, Duration::from_secs(150)))
}

/// Has the bot written to its log within `within`?
pub fn wrote_recently(root: &Path, bot: &str, within: Duration) -> bool {
    std::fs::read_dir(root.join("logs").join(bot))
        .ok()
        .and_then(|d| d.filter_map(|f| f.ok()).filter(|f| f.path().extension().map(|x| x == "jsonl").unwrap_or(false)).filter_map(|f| f.metadata().ok()?.modified().ok()).max())
        .and_then(|t| t.elapsed().ok())
        .map(|age| age <= within)
        .unwrap_or(false)
}

/// The ladder's standings, into the store's `ladder` table: what gives opponents their
/// rating and rank on the status page, and our own bots their exact rating. They are
/// fetched on GitHub (workflow `standings.yml`, every five minutes) and read here from the
/// repository's `standings` branch; this machine asks the ladder nothing.
pub fn refresh_ladder(root: &Path, store: &Store, ours: &[String]) -> Result<usize, String> {
    let dir = crate::store::data_dir(root).join("standings-remote");
    let git = |args: &[&str], cwd: &Path| -> Result<String, String> {
        let out = Command::new("git").current_dir(cwd).args(args).output().map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("git failed").to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if dir.join(".git").exists() {
        git(&["fetch", "--quiet", "--depth", "1", "origin", "standings"], &dir)?;
        git(&["reset", "--quiet", "--hard", "FETCH_HEAD"], &dir)?;
    } else {
        let url = git(&["remote", "get-url", "origin"], root)?;
        let _ = std::fs::create_dir_all(crate::store::data_dir(root));
        git(&["clone", "--quiet", "--single-branch", "--branch", "standings", "--depth", "1", &url, &dir.display().to_string()], root)?;
    }
    let text = std::fs::read_to_string(dir.join("standings.json")).map_err(|e| format!("standings.json: {e}"))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let list = v.get("leaderboard").and_then(Value::as_array).cloned().unwrap_or_default();
    let rows: Vec<(String, i64, f64, Option<String>, Option<i64>)> = list
        .iter()
        .enumerate()
        .filter_map(|(k, r)| {
            Some((
                r.get("name")?.as_str()?.to_string(),
                r.get("rank").and_then(Value::as_i64).unwrap_or(k as i64 + 1),
                r.get("rating")?.as_f64()?,
                r.get("version").and_then(Value::as_str).map(str::to_string),
                match (r.get("wins").and_then(Value::as_i64), r.get("losses").and_then(Value::as_i64)) {
                    (Some(w), Some(l)) => Some(w + l + r.get("draws").and_then(Value::as_i64).unwrap_or(0)),
                    _ => None,
                },
            ))
        })
        .collect();
    if rows.is_empty() {
        return Err("the standings file holds no rows".into());
    }
    // The same file twice changes nothing; say how old it is.
    let fetched = v.get("fetched").and_then(Value::as_str).unwrap_or("?").to_string();
    store.set_ladder(&rows, ours).map_err(|e| e.to_string())?;
    // Our bots' own pages carry a rating for every version, ranked or not: a new label (fewer
    // than 30 games) is not on the leaderboard, but it has a rating. That rating stands
    // after the latest game we have of that version.
    if let Some(bots) = v.get("bots").and_then(Value::as_object) {
        for (bot, b) in bots {
            for ver in b.get("versions").and_then(Value::as_array).into_iter().flatten() {
                let label = ["version", "label", "name", "number"].iter().find_map(|k| ver.get(*k).and_then(Value::as_str)).filter(|l| l.starts_with('v'));
                let rating = ver.get("rating").and_then(Value::as_f64);
                if let (Some(label), Some(rating)) = (label, rating) {
                    let games = match (ver.get("wins").and_then(Value::as_i64), ver.get("losses").and_then(Value::as_i64)) {
                        (Some(w), Some(l)) => Some(w + l + ver.get("draws").and_then(Value::as_i64).unwrap_or(0)),
                        _ => None,
                    };
                    let score = ver.get("leaderboard_score").or_else(|| ver.get("score")).and_then(Value::as_f64);
                    let _ = store.set_ladder_version(bot, label, rating, ver.get("rating_deviation").and_then(Value::as_f64), score, ver.get("rank").and_then(Value::as_i64), games);
                    let _ = store.conn.execute(
                        "UPDATE matches SET rating_after = ?3 WHERE bot = ?1 AND version = ?2 AND match_id =
                           (SELECT match_id FROM matches WHERE bot = ?1 AND version = ?2 AND result IS NOT NULL ORDER BY COALESCE(ended_ms, started_ms) DESC LIMIT 1)",
                        rusqlite::params![bot, label, rating],
                    );
                }
            }
        }
    }
    eprintln!("{} standings fetched on GitHub at {fetched}: {} rows", utc_stamp(), rows.len());
    Ok(rows.len())
}

/// Asks GitHub to run one of the repository's workflows now (`gh workflow run`), at most
/// once every `min_gap_s` for each workflow. GitHub's own schedule for these workflows did
/// not fire on 2026-11-18 (the standings ran once in two hours, the replays not at all), so
/// the Mac's health pass starts them itself. This talks to GitHub only, never to the ladder.
pub fn dispatch_workflow(root: &Path, workflow: &str, min_gap_s: u64) -> Result<bool, String> {
    let mark = crate::store::data_dir(root).join("run").join(format!("dispatched-{workflow}"));
    if let Some(age) = std::fs::metadata(&mark).ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()) {
        if age.as_secs() < min_gap_s {
            return Ok(false);
        }
    }
    let url = Command::new("git").current_dir(root).args(["remote", "get-url", "origin"]).output().map_err(|e| e.to_string())?;
    let url = String::from_utf8_lossy(&url.stdout).trim().to_string();
    let repo = url.trim_start_matches("git@github.com:").trim_start_matches("https://github.com/").trim_end_matches(".git").to_string();
    let gh = ["/opt/homebrew/bin/gh", "/usr/local/bin/gh"].into_iter().find(|p| Path::new(p).is_file()).unwrap_or("gh");
    let out = Command::new(gh).args(["workflow", "run", workflow, "--repo", &repo, "--ref", "main"]).output().map_err(|e| format!("gh: {e}"))?;
    let _ = std::fs::create_dir_all(mark.parent().unwrap_or(Path::new(".")));
    let _ = std::fs::write(&mark, utc_stamp());
    if out.status.success() {
        Ok(true)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn alert_log(root: &Path) -> std::path::PathBuf {
    root.join("logs").join("fleet-alerts.log")
}

/// One line in the alert log, the store and on the screen.
pub fn alert(root: &Path, kind: &str, bot: Option<&str>, text: &str) {
    let line = format!("{} {kind} {text}", utc_stamp());
    eprintln!("{line}");
    let _ = std::fs::create_dir_all(root.join("logs"));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(alert_log(root)) {
        let _ = writeln!(f, "{line}");
    }
    if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(2)) {
        store.event(bot, &format!("alert_{}", kind.to_lowercase()), &json!({ "text": text }));
    }
    // An optional hook carries the alert off the machine (a phone push, a message): if
    // `fleet/alert-hook.sh` exists and is executable it is run with the kind and the text
    // as its two arguments, in the background, so a slow hook never holds the watchdog.
    let hook = root.join("fleet").join("alert-hook.sh");
    if hook.is_file() {
        let _ = Command::new(&hook).arg(kind).arg(text).current_dir(root).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    }
    // The text goes to osascript as an argument, never into the script itself.
    let _ = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "display notification (item 1 of argv) with title \"Victris fleet\" sound name \"Basso\"", "-e", "end run", text])
        .output();
}

// ---- load guard -----------------------------------------------------------------------------
//
// The live bots share four fast cores with everything else on the machine. When they are
// being starved (2026-11-18: a bot was disqualified for ten silent ticks while batch jobs
// ran), batch work must stop. The guard looks every 15 seconds at the load average and at
// each live bot's recent think times, and keeps a pause file while either is too high.
// `fleet/duel.mjs` (and so the arena gate) starts no game while the file exists.

pub fn pause_file(root: &Path) -> std::path::PathBuf {
    root.join("data").join("run").join("batch.pause")
}

pub fn load_average() -> f64 {
    let mut avg = [0f64; 3];
    if unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) } < 1 {
        return 0.0;
    }
    avg[0]
}

/// 99th percentile of a bot's think time over the last 90 seconds, in milliseconds, from
/// the end of its tick log. `None` when it has not played enough ticks in that time.
pub fn recent_think_p99_ms(root: &Path, bot: &str) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let dir = root.join("logs").join(bot);
    let newest = std::fs::read_dir(&dir).ok()?.filter_map(|f| f.ok()).map(|f| f.path()).filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false)).max()?;
    let mut f = std::fs::File::open(newest).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(400_000))).ok()?;
    let mut text = String::new();
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    text.push_str(&String::from_utf8_lossy(&bytes));
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
    let mut us: Vec<i64> = text
        .lines()
        .filter(|l| l.contains("\"t\":\"tick\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("ts").and_then(Value::as_i64).map(|t| now - t <= 90_000).unwrap_or(false))
        .filter_map(|v| v.get("us").and_then(Value::as_i64))
        .collect();
    if us.len() < 40 {
        return None;
    }
    us.sort_unstable();
    Some(us[(us.len() - 1) * 99 / 100] as f64 / 1000.0)
}

/// Seconds since the bot last answered a turn, from the end of its log. This is what says
/// whether a bot is playing right now: a process can be up, connected and queued, and
/// still be given no match.
pub fn last_turn_age_s(root: &Path, bot: &str) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let dir = root.join("logs").join(bot);
    let newest = std::fs::read_dir(&dir).ok()?.filter_map(|f| f.ok()).map(|f| f.path()).filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false)).max()?;
    let mut f = std::fs::File::open(newest).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(60_000))).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let ts = text.lines().rev().filter(|l| l.contains("\"t\":\"tick\"")).find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("ts")?.as_i64())?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis() as i64;
    Some((now - ts).max(0) as f64 / 1000.0)
}

/// Connections the ladder dropped on our bots in the last `window_ms`, counted from the
/// ends of their logs (`disconnected` lines whose reason is a reset or a closed socket).
pub fn recent_resets(root: &Path, bots: &[String], window_ms: i64) -> usize {
    use std::io::{Read, Seek, SeekFrom};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let mut n = 0;
    for bot in bots {
        let dir = root.join("logs").join(bot);
        let Some(newest) = std::fs::read_dir(&dir).ok().and_then(|d| d.filter_map(|f| f.ok()).map(|f| f.path()).filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false)).max()) else { continue };
        let Ok(mut f) = std::fs::File::open(newest) else { continue };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if f.seek(SeekFrom::Start(len.saturating_sub(1_500_000))).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if f.read_to_end(&mut bytes).is_err() {
            continue;
        }
        n += String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| l.contains("\"t\":\"disconnected\"") && (l.contains("reset") || l.contains("closed the connection") || l.contains("Handshake")))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("ts").and_then(Value::as_i64).map(|t| now - t <= window_ms).unwrap_or(false))
            .count();
    }
    n
}

/// Why batch work should stand still right now, if it should.
pub fn overload(root: &Path, cfg: &Config) -> Option<String> {
    // Memory and disk first: a machine that pages stalls every process, the bots' too.
    if let Some(p) = crate::machine::read(root).problems(cfg).into_iter().next() {
        return Some(p);
    }
    // A process kept waiting for the CPU is the harm itself, whatever caused it.
    let wait = crate::machine::last_worst_stall();
    if wait as f64 > cfg.guard_max_cpu_wait_ms {
        return Some(format!("a process waited {wait} ms for the CPU (limit {:.0} ms)", cfg.guard_max_cpu_wait_ms));
    }
    let load = load_average();
    if load > cfg.guard_load {
        return Some(format!("load average {load:.1} is above {:.0}", cfg.guard_load));
    }
    for b in cfg.bots.iter().filter(|b| b.expect_online) {
        if let Some(p99) = recent_think_p99_ms(root, &b.name) {
            if p99 > cfg.guard_think_p99_ms {
                return Some(format!("{} think time p99 is {p99:.0} ms, above {:.0} ms", b.name, cfg.guard_think_p99_ms));
            }
        }
    }
    None
}

/// A line in the alert log without a notification on screen.
pub fn log_line(root: &Path, kind: &str, text: &str) {
    let line = format!("{} {kind} {text}", utc_stamp());
    eprintln!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(alert_log(root)) {
        let _ = writeln!(f, "{line}");
    }
}

// ---- stall check ----------------------------------------------------------------------------
//
// "Online" is not "playing". On 2026-11-18 the ladder gave none of our five bots a match
// for four minutes (each was refused with the owner's match limit right after its game),
// every process was up and connected, and a person looking at the status page saw it
// first. A bot that has answered no turn for two minutes is stalled: say so, say why as
// far as its log tells, restart it when that can help, and say when it is over.

/// A bot is stalled when it has answered no turn for this long.
pub const STALL_AFTER_S: f64 = 120.0;

/// Why a bot has answered no turn lately, read from the end of its log: what happened
/// after its last turn. Plain words for the alert log and the status page.
pub fn stall_cause(root: &Path, bot: &str) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let dir = root.join("logs").join(bot);
    let Some(newest) = std::fs::read_dir(&dir).ok().and_then(|d| d.filter_map(|f| f.ok()).map(|f| f.path()).filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false)).max()) else {
        return "it has no log".into();
    };
    let Ok(mut f) = std::fs::File::open(newest) else { return "its log cannot be read".into() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(60_000)));
    let mut bytes = Vec::new();
    let _ = f.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let after = lines.iter().rposition(|l| l.contains("\"t\":\"tick\"")).map(|k| k + 1).unwrap_or(0);
    let mut refused: Option<String> = None;
    let mut drops = 0;
    let mut started = false;
    for v in lines[after..].iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        match v.get("t").and_then(Value::as_str) {
            Some("server_error") => {
                let d = v.get("detail");
                let code = d.and_then(|d| d.get("code")).and_then(Value::as_str).unwrap_or("?");
                let msg = d.and_then(|d| d.get("message")).and_then(Value::as_str).unwrap_or("");
                refused = Some(if msg.is_empty() { code.to_string() } else { format!("{code}: {msg}") });
            }
            Some("disconnected") => drops += 1,
            Some("hello") => started = true,
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if let Some(r) = refused {
        parts.push(format!("the ladder refused to queue it ({r})"));
    }
    if drops > 0 {
        parts.push(format!("its connection was dropped {drops} time{} since", if drops == 1 { "" } else { "s" }));
    }
    if started {
        parts.push("a match was started but no turn has been answered".to_string());
    }
    if parts.is_empty() {
        "it is connected and queued, and the ladder has given it no match".to_string()
    } else {
        parts.join("; ")
    }
}

/// The bots that are stalled right now, each with how long and why. For the status page.
pub fn stalls(root: &Path, cfg: &Config) -> Vec<(String, f64, String)> {
    cfg.bots
        .iter()
        .filter(|b| b.expect_online)
        .filter_map(|b| {
            let age = last_turn_age_s(root, &b.name)?;
            (age >= STALL_AFTER_S).then(|| (b.name.clone(), age, stall_cause(root, &b.name)))
        })
        .collect()
}

#[derive(Default)]
struct Stalls {
    /// when each stalled bot was first reported
    since: HashMap<String, Instant>,
    /// when each bot was last restarted for a stall
    acted: HashMap<String, Instant>,
    last_action: Option<Instant>,
    fleet_wide: bool,
}

impl Stalls {
    fn check(&mut self, root: &Path, cfg: &Config) {
        let now = stalls(root, cfg);
        let watched = cfg.bots.iter().filter(|b| b.expect_online).count();
        let store = Store::open(&default_db(root), Duration::from_secs(2)).ok();
        // Over: a bot that was reported answers turns again.
        let over: Vec<String> = self.since.keys().filter(|b| !now.iter().any(|(n, _, _)| n == *b)).cloned().collect();
        for bot in over {
            let took = self.since.remove(&bot).map(|t| t.elapsed().as_secs()).unwrap_or(0);
            log_line(root, "STALL", &format!("over: {bot} answers turns again, about {} s after it was reported", took));
            if let Some(s) = &store {
                s.event(Some(&bot), "stall_over", &json!({ "after_report_s": took }));
            }
        }
        // Most of the fleet at once is not a bot's fault: restarting them all would only
        // open a row of new connections, which the ladder throttles.
        let wide = watched >= 3 && now.len() * 3 >= watched * 2;
        if wide && !self.fleet_wide {
            log_line(root, "STALL", &format!("{} of {watched} bots have no match at once: this is on the ladder's side or the owner's match limit, not one bot. No bot is restarted for it.", now.len()));
        }
        self.fleet_wide = wide;
        for (bot, age, cause) in &now {
            if !self.since.contains_key(bot) {
                self.since.insert(bot.clone(), Instant::now());
                log_line(root, "STALL", &format!("{bot} has answered no turn for {age:.0} s: {cause}"));
                if let Some(s) = &store {
                    s.event(Some(bot), "stall", &json!({ "no_turn_s": age.round(), "cause": cause }));
                }
            }
            // What can be fixed from here: a bot of ours that alone sits without a match gets
            // a fresh process, connection and queue request. One bot at a time, each at most
            // once in ten minutes. Never the flagship (its own thread runs it), never a bot
            // behind the gateway (the gateway holds its connection).
            let Some(b) = cfg.bot(bot) else { continue };
            let may = cfg.stall_restarts && b.managed && !b.gateway && b.role != "flagship" && !wide;
            let rested = self.acted.get(bot).map(|t| t.elapsed() >= Duration::from_secs(600)).unwrap_or(true);
            let spaced = self.last_action.map(|t| t.elapsed() >= Duration::from_secs(60)).unwrap_or(true);
            if may && rested && spaced && *age >= STALL_AFTER_S + 30.0 {
                if let Some(pid) = crate::supervise::bot_pid(root, bot) {
                    if crate::supervise::signal(pid, libc::SIGTERM) {
                        self.acted.insert(bot.clone(), Instant::now());
                        self.last_action = Some(Instant::now());
                        log_line(root, "STALL", &format!("{bot}: restarted (pid {pid}) for a fresh connection and a new queue request"));
                        if let Some(s) = &store {
                            s.event(Some(bot), "stall_restart", &json!({ "pid": pid, "no_turn_s": age.round() }));
                        }
                    }
                }
            }
        }
    }
}

struct Guard {
    /// the machine (memory, swap, disk) was short at the last check
    machine_short: bool,
    paused: bool,
    calm: u32,
    /// a burst of reset connections is going on
    resetting: bool,
    reset_calm: u32,
}

impl Guard {
    /// Bursts of reset connections are written down as they start and end, so they can be
    /// lined up later with whatever else was happening on this network.
    fn note_resets(&mut self, root: &Path, cfg: &Config) {
        let bots: Vec<String> = cfg.bots.iter().filter(|b| b.expect_online).map(|b| b.name.clone()).collect();
        let n = recent_resets(root, &bots, 180_000);
        if n >= 4 {
            self.reset_calm = 0;
            if !self.resetting {
                self.resetting = true;
                // Was it our own network? Ask the router and a neutral host right now.
                let ping = |host: &str| -> String {
                    match Command::new("/sbin/ping").args(["-c", "3", "-i", "0.2", "-t", "3", "-q", host]).output() {
                        Ok(o) => {
                            let text = String::from_utf8_lossy(&o.stdout).to_string();
                            let loss = text.lines().find(|l| l.contains("packet loss")).and_then(|l| l.split(", ").find(|p| p.contains("packet loss")).map(str::to_string)).unwrap_or_else(|| "no answer".into());
                            let rtt = text.lines().find(|l| l.contains("min/avg/max")).and_then(|l| l.split('=').nth(1)).and_then(|v| v.trim().split('/').nth(1)).map(|a| format!(", {a} ms")).unwrap_or_default();
                            format!("{loss}{rtt}")
                        }
                        Err(_) => "ping failed".into(),
                    }
                };
                let gateway = Command::new("/sbin/route").args(["-n", "get", "default"]).output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).lines().find(|l| l.contains("gateway:")).and_then(|l| l.split_whitespace().nth(1)).map(str::to_string));
                let router = gateway.as_deref().map(ping).unwrap_or_else(|| "no default route".into());
                log_line(root, "RESETS", &format!("the ladder dropped {n} of our bots' connections in the last 3 minutes; router: {router}; 1.1.1.1: {}", ping("1.1.1.1")));
            }
        } else if self.resetting {
            self.reset_calm += 1;
            // Three minutes with fewer than four in the window.
            if self.reset_calm >= 12 {
                self.resetting = false;
                log_line(root, "RESETS", "the burst of dropped connections is over");
            }
        }
    }

    /// The machine's reading, as the `machine` row of `gateway_state`, and an alert line when
    /// it becomes short of memory or disk and when it recovers.
    fn note_machine(&mut self, root: &Path, cfg: &Config) {
        let m = crate::machine::read(root);
        let stall = crate::machine::take_worst_stall();
        let problems = m.problems(cfg);
        let short = !problems.is_empty();
        let verdict = if short { problems.join("; ") } else { m.summary() };
        if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(2)) {
            let _ = store.conn.execute(
                "INSERT INTO gateway_state (bot, ts_ms, level, verdict, socket_id, detail) VALUES ('machine', ?1, ?2, ?3, NULL, ?4)
                 ON CONFLICT(bot) DO UPDATE SET ts_ms = ?1, level = ?2, verdict = ?3, socket_id = NULL, detail = ?4",
                rusqlite::params![crate::store::now_ms(), if short { "bad" } else if stall > 500 { "warn" } else { "ok" }, verdict, m.detail(stall).to_string()],
            );
        }
        if stall > 500 {
            log_line(root, "STALL", &format!("a process waited {stall} ms for the CPU in the last 15 s ({})", m.summary()));
        }
        if short && !self.machine_short {
            log_line(root, "ALERT", &format!("the machine is short: {verdict}; batch work is paused until it recovers"));
        } else if !short && self.machine_short {
            log_line(root, "RECOVERED", &format!("the machine has room again: {}", m.summary()));
        }
        self.machine_short = short;
    }

    fn check(&mut self, root: &Path, cfg: &Config) {
        self.note_resets(root, cfg);
        self.note_machine(root, cfg);
        match overload(root, cfg) {
            Some(why) => {
                self.calm = 0;
                let _ = std::fs::create_dir_all(root.join("data").join("run"));
                let _ = std::fs::write(pause_file(root), format!("{} {why}\n", utc_stamp()));
                if !self.paused {
                    self.paused = true;
                    log_line(root, "LOAD", &format!("batch work paused: {why}"));
                }
            }
            None => {
                self.calm += 1;
                // Four calm checks in a row (a minute) before batch work may go on.
                if self.calm >= 4 && (self.paused || pause_file(root).exists()) {
                    let _ = std::fs::remove_file(pause_file(root));
                    if self.paused {
                        log_line(root, "LOAD", "batch work may go on: the machine is calm again");
                    }
                    self.paused = false;
                }
            }
        }
    }
}

#[derive(Default)]
struct Watch {
    offline_since: Option<Instant>,
    alerted_at: Option<Instant>,
}

pub fn run(root: &Path, stop: &dyn Fn() -> bool) {
    let mut bots: HashMap<String, Watch> = HashMap::new();
    let mut api_down_since: Option<Instant> = None;
    let mut api_alerted = false;
    crate::machine::start_stall_probe();
    let mut guard = Guard { machine_short: false, paused: false, calm: 0, resetting: false, reset_calm: 0 };
    let mut stalled = Stalls::default();
    eprintln!("{} watchdog started", utc_stamp());
    while !stop() {
        // The list is read each round, so a config edit needs no restart.
        let cfg = Config::load(root).unwrap_or_default();
        let (mut asked, mut failed) = (0, 0);
        for b in cfg.bots.iter().filter(|b| b.expect_online) {
            asked += 1;
            let w = bots.entry(b.name.clone()).or_default();
            // A bot that wrote to its log in the last two and a half minutes is on the
            // ladder: it logs every turn and every match start. Only a silent bot is worth
            // a question to the ladder, which keeps requests from this address near zero
            // (they appear to count against the same limit as the bots' own connections).
            let answer = is_online(root, &b.name);
            match answer {
                Ok(true) => {
                    if w.alerted_at.is_some() {
                        let down = w.offline_since.map(|t| t.elapsed().as_secs() / 60).unwrap_or(0);
                        alert(root, "RECOVERED", Some(&b.name), &format!("{} is back online after about {down} min", b.name));
                    }
                    *w = Watch::default();
                }
                Ok(false) => {
                    let since = *w.offline_since.get_or_insert_with(Instant::now);
                    let due = match w.alerted_at {
                        None => since.elapsed() >= OFFLINE_AFTER,
                        Some(t) => t.elapsed() >= REPEAT_EVERY,
                    };
                    if due {
                        w.alerted_at = Some(Instant::now());
                        let who = b.owner.as_deref().map(|o| format!(" Owner: {o}.")).unwrap_or_default();
                        alert(root, "ALERT", Some(&b.name), &format!("{} has written nothing to its log for {} min: it is not playing.{who}", b.name, 2 + since.elapsed().as_secs() / 60));
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("{} could not ask about {}: {e}", utc_stamp(), b.name);
                }
            }
        }
        // Connected is not the same as playing: a bot can sit on the ladder for an hour
        // being refused (2026-11-18: a version label the ladder would no longer take).
        // No finished game in fifteen minutes is an alert of its own.
        if let Ok(store) = Store::open(&default_db(root), Duration::from_secs(2)) {
            for b in cfg.bots.iter().filter(|b| b.expect_online) {
                let last: Option<i64> = store.conn.query_row("SELECT MAX(COALESCE(ended_ms, started_ms)) FROM matches WHERE bot = ?1", [&b.name], |r| r.get(0)).ok().flatten();
                let Some(last) = last else { continue };
                let idle_min = (crate::store::now_ms() - last) / 60_000;
                let key = format!("{}#idle", b.name);
                let w = bots.entry(key).or_default();
                if idle_min >= 15 {
                    let due = w.alerted_at.map(|t| t.elapsed() >= REPEAT_EVERY).unwrap_or(true);
                    if due {
                        w.alerted_at = Some(Instant::now());
                        let who = b.owner.as_deref().map(|o| format!(" Owner: {o}.")).unwrap_or_default();
                        alert(root, "ALERT", Some(&b.name), &format!("{} has not finished a game for {idle_min} min, though it may be connected. See its log.{who}", b.name));
                    }
                } else if w.alerted_at.is_some() {
                    alert(root, "RECOVERED", Some(&b.name), &format!("{} is playing again", b.name));
                    *w = Watch::default();
                }
            }
        }

        // Not being able to ask at all is its own alert: the network or the ladder is down.
        if asked > 0 && failed == asked {
            let since = *api_down_since.get_or_insert_with(Instant::now);
            if !api_alerted && since.elapsed() >= Duration::from_secs(300) {
                api_alerted = true;
                alert(root, "ALERT", None, "the ladder's API has not answered for 5 minutes: the network or the ladder is down, and bot status is unknown");
            }
        } else {
            if api_alerted {
                alert(root, "RECOVERED", None, "the ladder's API answers again");
            }
            api_down_since = None;
            api_alerted = false;
        }
        let until = Instant::now() + CHECK_EVERY;
        let mut next_guard = Instant::now();
        while Instant::now() < until && !stop() {
            if Instant::now() >= next_guard {
                next_guard = Instant::now() + Duration::from_secs(15);
                guard.check(root, &cfg);
                stalled.check(root, &cfg);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
