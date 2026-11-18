//! The offline watchdog: asks the ladder, once a minute, whether each bot that should be
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
    // The text goes to osascript as an argument, never into the script itself.
    let _ = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "display notification (item 1 of argv) with title \"Victris fleet\" sound name \"Basso\"", "-e", "end run", text])
        .output();
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
    eprintln!("{} watchdog started", utc_stamp());
    while !stop() {
        // The list is read each round, so a config edit needs no restart.
        let cfg = Config::load(root).unwrap_or_default();
        let (mut asked, mut failed) = (0, 0);
        for b in cfg.bots.iter().filter(|b| b.expect_online) {
            asked += 1;
            let w = bots.entry(b.name.clone()).or_default();
            match is_online(&b.name) {
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
        while Instant::now() < until && !stop() {
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}
