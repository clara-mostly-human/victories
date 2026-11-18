//! The offline watchdog and the load guard. The watchdog asks the ladder, once a minute, whether each bot that should be
//! up is online, and raises an alert when one has been offline for two minutes.
//!
//! It only reads the ladder's public API. It never starts, stops or connects as any bot:
//! bringing a bot back is its owner's job. An alert is one loud line in
//! `logs/fleet-alerts.log`, an event in the store and a notification on the Mac; it is
//! repeated every 15 minutes while the bot stays down, and a recovery line follows when it
//! is back.

use crate::collect::HOST;
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

/// Is the bot online, according to the ladder? `Err` when the ladder could not be asked.
pub fn is_online(bot: &str) -> Result<bool, String> {
    if !bot.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("a bot name the watchdog will not put in a URL".into());
    }
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "20", "-A", "fleet-watch/1.0", "-w", "\n%{http_code}", &format!("{HOST}/api/bots/{bot}")])
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let cut = out.stdout.iter().rposition(|b| *b == b'\n').unwrap_or(0);
    let code = String::from_utf8_lossy(&out.stdout[cut..]).trim().to_string();
    if code != "200" {
        return Err(format!("HTTP {code}"));
    }
    let v: Value = serde_json::from_slice(&out.stdout[..cut]).map_err(|e| e.to_string())?;
    v.get("online").and_then(Value::as_bool).ok_or_else(|| "the answer has no `online` field".to_string())
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

struct Guard {
    paused: bool,
    calm: u32,
}

impl Guard {
    fn check(&mut self, root: &Path, cfg: &Config) {
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
    let mut guard = Guard { paused: false, calm: 0 };
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
            let answer = if wrote_recently(root, &b.name, Duration::from_secs(150)) { Ok(true) } else { is_online(&b.name) };
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
                        alert(root, "ALERT", Some(&b.name), &format!("{} is OFFLINE on the ladder, for {} min now.{who}", b.name, since.elapsed().as_secs() / 60));
                    }
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("{} could not ask about {}: {e}", utc_stamp(), b.name);
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
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
