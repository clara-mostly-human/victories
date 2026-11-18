//! The state of the fleet in one place, as a page (`data/status.html`, rewritten every five
//! minutes by the health pass) and as a short text digest (`fleet digest`).

use crate::config::Config;
use crate::online::{alert_log, load_average, pause_file, recent_think_p99_ms, utc_stamp};
use crate::stats::{Filter, Record};
use crate::store::{now_ms, Store};
use crate::supervise::bot_pid;
use std::path::Path;

pub struct BotLine {
    pub name: String,
    pub role: String,
    pub managed: bool,
    pub running: Option<bool>,
    pub halted: Option<String>,
    pub release: String,
    pub version: String,
    pub day: Record,
    pub hour_games: i64,
    pub rating: Option<f64>,
    pub rating_day: f64,
    pub last_game_ms: Option<i64>,
    pub think_p99_ms: Option<f64>,
}

pub struct ArmLine {
    pub experiment: String,
    pub arm: String,
    pub record: Record,
    pub target: i64,
}

pub struct ReleaseLine {
    pub id: String,
    pub status: String,
    pub gate: String,
    pub evidence: String,
    pub note: String,
}

pub struct Snapshot {
    pub at: String,
    pub bots: Vec<BotLine>,
    pub arms: Vec<ArmLine>,
    pub releases: Vec<ReleaseLine>,
    pub live: String,
    pub corpus_ok: i64,
    pub corpus_day: i64,
    pub corpus_queued: i64,
    pub corpus_strong: i64,
    pub alerts_day: usize,
    pub alerts: Vec<String>,
    pub load: f64,
    pub paused: Option<String>,
}

fn one<T: rusqlite::types::FromSql>(store: &Store, sql: &str, p: impl rusqlite::Params) -> Option<T> {
    store.conn.query_row(sql, p, |r| r.get::<_, Option<T>>(0)).ok().flatten()
}

pub fn snapshot(root: &Path, store: &Store, cfg: &Config) -> Snapshot {
    let now = now_ms();
    let day = now - 86_400_000;
    let mut bots = Vec::new();
    for b in &cfg.bots {
        let st = store.bot_state(&b.name).ok().flatten();
        let rec = store
            .records("all", &Filter { bot: Some(b.name.clone()), since_ms: Some(day), ..Filter::default() })
            .ok()
            .and_then(|r| r.first().map(|r| (r.record, r.rating)));
        let last = |col: &str| -> Option<String> { one(store, &format!("SELECT {col} FROM matches WHERE bot = ?1 AND {col} IS NOT NULL ORDER BY started_ms DESC LIMIT 1"), [&b.name]) };
        // The ladder's list gives the rating before a game and the change it brought.
        let rating: Option<f64> = store
            .conn
            .query_row(
                "SELECT rating_before + COALESCE(rating_change, 0) FROM matches WHERE bot = ?1 AND rating_before IS NOT NULL ORDER BY started_ms DESC LIMIT 1",
                [&b.name],
                |r| r.get(0),
            )
            .ok();
        bots.push(BotLine {
            name: b.name.clone(),
            role: b.role.clone(),
            managed: b.managed,
            running: b.managed.then(|| bot_pid(root, &b.name).is_some()),
            halted: st.as_ref().and_then(|s| s.halted.clone()),
            release: st.as_ref().and_then(|s| s.release.clone()).or_else(|| last("release")).unwrap_or_else(|| "-".into()),
            version: st.as_ref().and_then(|s| s.version.clone()).or_else(|| last("version")).unwrap_or_else(|| "-".into()),
            day: rec.map(|r| r.0).unwrap_or_default(),
            hour_games: one(store, "SELECT COUNT(*) FROM matches WHERE bot = ?1 AND result IS NOT NULL AND started_ms >= ?2", rusqlite::params![b.name, now - 3_600_000]).unwrap_or(0),
            rating,
            rating_day: rec.map(|r| r.1).unwrap_or(0.0),
            last_game_ms: one(store, "SELECT MAX(COALESCE(ended_ms, started_ms)) FROM matches WHERE bot = ?1", [&b.name]),
            think_p99_ms: recent_think_p99_ms(root, &b.name),
        });
    }
    let mut arms = Vec::new();
    for e in store.experiments(true).unwrap_or_default() {
        let rows = store.records("arm", &Filter { experiment: Some(e.name.clone()), ..Filter::default() }).unwrap_or_default();
        for (arm, _) in &e.arms {
            let record = rows.iter().find(|r| r.key == *arm).map(|r| r.record).unwrap_or_default();
            arms.push(ArmLine { experiment: e.name.clone(), arm: arm.clone(), record, target: e.games_per_arm });
        }
    }
    let mut releases: Vec<ReleaseLine> = store
        .releases()
        .unwrap_or_default()
        .into_iter()
        .rev()
        .take(6)
        .map(|r| ReleaseLine {
            gate: r.gate.as_ref().map(|g| if g["games"].is_null() { g["verdict"].as_str().unwrap_or("?").to_string() } else { format!("{} at {:.0}% of {}", g["verdict"].as_str().unwrap_or("?"), 100.0 * g["score"].as_f64().unwrap_or(0.0), g["games"]) }).unwrap_or_else(|| "-".into()),
            evidence: r.evidence.as_ref().map(|v| format!("{}, {} games, {:+.1} pts", v["verdict"].as_str().unwrap_or("?"), v["candidate"]["games"], 100.0 * v["score_difference"].as_f64().unwrap_or(0.0))).unwrap_or_else(|| "-".into()),
            note: r.note.clone().unwrap_or_default(),
            id: r.id,
            status: r.status,
        })
        .collect();
    releases.truncate(6);
    let text = std::fs::read_to_string(alert_log(root)).unwrap_or_default();
    let since = {
        // Alert lines start with a UTC stamp, which sorts as text.
        let d = now / 1000 - 86_400;
        let (days, rem) = (d.div_euclid(86_400), d.rem_euclid(86_400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let dd = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
        format!("{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
    };
    let lines: Vec<&str> = text.lines().collect();
    Snapshot {
        at: utc_stamp(),
        bots,
        arms,
        releases,
        live: store.live_release().ok().flatten().map(|r| r.id).unwrap_or_else(|| "none".into()),
        corpus_ok: one(store, "SELECT COUNT(*) FROM replays WHERE status = 'ok'", []).unwrap_or(0),
        corpus_day: one(store, "SELECT COUNT(*) FROM replays WHERE status = 'ok' AND fetched_ms >= ?1", [day]).unwrap_or(0),
        corpus_queued: one(store, "SELECT COUNT(*) FROM replays WHERE status = 'queued'", []).unwrap_or(0),
        corpus_strong: one(store, "SELECT COUNT(*) FROM replays WHERE status = 'ok' AND rating1 >= 2400 AND rating2 >= 2400", []).unwrap_or(0),
        alerts_day: lines.iter().filter(|l| l.contains(" ALERT ") && **l >= since.as_str()).count(),
        alerts: lines.iter().rev().take(12).map(|l| l.to_string()).collect(),
        load: load_average(),
        paused: std::fs::read_to_string(pause_file(root)).ok().map(|s| s.trim().to_string()),
    }
}

fn state(b: &BotLine) -> (&'static str, &'static str) {
    let stale = b.last_game_ms.map(|t| now_ms() - t > 900_000).unwrap_or(true);
    match (b.halted.is_some(), b.running, stale) {
        (true, _, _) => ("halted", "bad"),
        (_, Some(false), _) => ("down", "bad"),
        (_, _, true) => ("quiet", "warn"),
        _ => ("playing", "good"),
    }
}

/// The digest: the first line fits a phone notification; the rest is the day in brief.
pub fn digest(s: &Snapshot) -> String {
    let trouble: Vec<String> = s.bots.iter().filter(|b| state(b).1 == "bad").map(|b| format!("{} {}", b.name, state(b).0)).collect();
    let games: i64 = s.bots.iter().map(|b| b.day.games()).sum();
    let mut out = format!(
        "Fleet: {}; {} games in 24 h; {} alerts; corpus {}.\n\n",
        if trouble.is_empty() { format!("all {} bots up", s.bots.len()) } else { trouble.join(", ") },
        games,
        s.alerts_day,
        s.corpus_ok
    );
    for b in &s.bots {
        out.push_str(&format!(
            "{} ({}, {} {}): {} in 24 h, {}W {}L {}D ({:.0}%), rating {} ({:+.0} today)\n",
            b.name,
            b.role,
            b.release,
            b.version,
            b.day.games(),
            b.day.wins,
            b.day.losses,
            b.day.draws,
            100.0 * b.day.score(),
            b.rating.map(|r| format!("{r:.0}")).unwrap_or_else(|| "?".into()),
            b.rating_day
        ));
    }
    out.push_str(&format!("\nLive release: {}\n", s.live));
    for r in s.releases.iter().take(3) {
        out.push_str(&format!("  {} is {}; gate {}; ladder {}\n", r.id, r.status, r.gate, r.evidence));
    }
    if !s.arms.is_empty() {
        out.push_str("\nExperiments\n");
        for a in &s.arms {
            out.push_str(&format!("  {} / {}: {} of {} games, {}W {}L {}D ({:.0}%)\n", a.experiment, a.arm, a.record.games(), a.target, a.record.wins, a.record.losses, a.record.draws, 100.0 * a.record.score()));
        }
    }
    out.push_str(&format!("\nCorpus: {} verified (+{} in 24 h), {} between bots rated 2400 and up, {} waiting\n", s.corpus_ok, s.corpus_day, s.corpus_strong, s.corpus_queued));
    if let Some(p) = &s.paused {
        out.push_str(&format!("Batch work is paused: {p}\n"));
    }
    out
}

fn esc(t: &str) -> String {
    t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn html(s: &Snapshot) -> String {
    let mut bots = String::new();
    for b in &s.bots {
        let (word, class) = state(b);
        let (lo, hi) = b.day.interval();
        bots.push_str(&format!(
            "<tr><td><strong>{}</strong><span class=\"sub\">{}</span></td><td><span class=\"pill {class}\">{word}</span></td><td>{}<span class=\"sub\">{}</span></td><td class=\"n\">{}</td><td class=\"n\">{}<span class=\"sub\">{:+.0} today</span></td><td class=\"n\">{}<span class=\"sub\">{} last hour</span></td><td class=\"n\">{}&ndash;{}&ndash;{}</td><td class=\"n\">{}<span class=\"sub\">{}</span></td><td class=\"n\">{}</td></tr>\n",
            esc(&b.name),
            esc(&b.role),
            esc(&b.release),
            if b.managed { "run by the fleet" } else { "run by its own thread" },
            esc(&b.version),
            b.rating.map(|r| format!("{r:.0}")).unwrap_or_else(|| "&ndash;".into()),
            b.rating_day,
            b.day.games(),
            b.hour_games,
            b.day.wins,
            b.day.losses,
            b.day.draws,
            if b.day.games() > 0 { format!("{:.0}%", 100.0 * b.day.score()) } else { "&ndash;".into() },
            if b.day.games() > 0 { format!("{:.0} to {:.0}%", 100.0 * lo, 100.0 * hi) } else { String::new() },
            b.think_p99_ms.map(|p| format!("{p:.0} ms")).unwrap_or_else(|| "&ndash;".into()),
        ));
    }
    let mut arms = String::new();
    for a in &s.arms {
        let pct = (100 * a.record.games() / a.target.max(1)).min(100);
        arms.push_str(&format!(
            "<tr><td><strong>{}</strong></td><td>{}</td><td class=\"n\">{} of {}<div class=\"bar\"><i style=\"width:{pct}%\"></i></div></td><td class=\"n\">{}&ndash;{}&ndash;{}</td><td class=\"n\">{}</td></tr>\n",
            esc(&a.experiment),
            esc(&a.arm),
            a.record.games(),
            a.target,
            a.record.wins,
            a.record.losses,
            a.record.draws,
            if a.record.games() > 0 { format!("{:.0}%", 100.0 * a.record.score()) } else { "&ndash;".into() }
        ));
    }
    if arms.is_empty() {
        arms = "<tr><td colspan=\"5\" class=\"sub\">No experiment is running.</td></tr>".into();
    }
    let mut releases = String::new();
    for r in &s.releases {
        let class = match r.status.as_str() {
            "live" => "good",
            "rejected" => "bad",
            "canary" | "proven" | "gated" => "warn",
            _ => "",
        };
        releases.push_str(&format!(
            "<tr><td><strong>{}</strong><span class=\"sub\">{}</span></td><td><span class=\"pill {class}\">{}</span></td><td>{}</td><td>{}</td></tr>\n",
            esc(&r.id),
            esc(&r.note),
            esc(&r.status),
            esc(&r.gate),
            esc(&r.evidence)
        ));
    }
    let alerts: String = if s.alerts.is_empty() { "<li class=\"sub\">Nothing yet.</li>".into() } else { s.alerts.iter().map(|l| format!("<li>{}</li>", esc(l))).collect() };
    let up = s.bots.iter().filter(|b| state(b).1 != "bad").count();
    let games: i64 = s.bots.iter().map(|b| b.day.games()).sum();
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Victris Fleet Status</title>
<style>
:root {{
  --bg: #f6f4ef; --panel: #fffdf8; --ink: #1d1b16; --muted: #6b665c; --line: #e2ddd1;
  --good: #1f7a4d; --good-bg: #dff1e6; --warn: #8a5a00; --warn-bg: #fbecc8; --bad: #a3271f; --bad-bg: #f8dcd8; --accent: #2f5d8a;
}}
@media (prefers-color-scheme: dark) {{
  :root:not([data-theme="light"]) {{
    --bg: #14130f; --panel: #1d1b16; --ink: #ece8dd; --muted: #a09a8c; --line: #343027;
    --good: #7fd3a4; --good-bg: #173326; --warn: #f0c060; --warn-bg: #3a2c0c; --bad: #f09088; --bad-bg: #401a16; --accent: #8fb8e0;
  }}
}}
:root[data-theme="dark"] {{
  --bg: #14130f; --panel: #1d1b16; --ink: #ece8dd; --muted: #a09a8c; --line: #343027;
  --good: #7fd3a4; --good-bg: #173326; --warn: #f0c060; --warn-bg: #3a2c0c; --bad: #f09088; --bad-bg: #401a16; --accent: #8fb8e0;
}}
* {{ box-sizing: border-box; }}
body {{ margin: 0; background: var(--bg); color: var(--ink); font: 15px/1.45 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }}
main {{ max-width: 1080px; margin: 0 auto; padding: 28px 16px 48px; }}
header {{ display: flex; flex-wrap: wrap; align-items: baseline; justify-content: space-between; gap: 8px; margin-bottom: 20px; }}
h1 {{ font-size: 26px; margin: 0; letter-spacing: -0.01em; }}
h2 {{ font-size: 13px; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); margin: 28px 0 8px; }}
.stamp {{ color: var(--muted); font-size: 13px; font-variant-numeric: tabular-nums; }}
.tiles {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(150px, 1fr)); gap: 10px; }}
.tile {{ background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 12px 14px; }}
.tile b {{ display: block; font-size: 24px; font-variant-numeric: tabular-nums; }}
.tile span {{ color: var(--muted); font-size: 13px; }}
.wrap {{ background: var(--panel); border: 1px solid var(--line); border-radius: 10px; overflow-x: auto; }}
table {{ width: 100%; border-collapse: collapse; min-width: 640px; }}
th, td {{ padding: 9px 12px; text-align: left; border-bottom: 1px solid var(--line); vertical-align: top; }}
th {{ font-size: 12px; font-weight: 600; color: var(--muted); white-space: nowrap; }}
tr:last-child td {{ border-bottom: 0; }}
.n {{ text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }}
th.n {{ text-align: right; }}
.sub {{ display: block; color: var(--muted); font-size: 12px; font-weight: 400; }}
.pill {{ display: inline-block; padding: 1px 9px; border-radius: 999px; font-size: 12px; font-weight: 600; background: var(--line); color: var(--ink); }}
.pill.good {{ background: var(--good-bg); color: var(--good); }}
.pill.warn {{ background: var(--warn-bg); color: var(--warn); }}
.pill.bad {{ background: var(--bad-bg); color: var(--bad); }}
.bar {{ height: 4px; background: var(--line); border-radius: 2px; margin-top: 5px; min-width: 90px; }}
.bar i {{ display: block; height: 4px; background: var(--accent); border-radius: 2px; }}
ul {{ list-style: none; margin: 0; padding: 10px 14px; font: 12px/1.6 ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }}
.note {{ color: var(--muted); font-size: 13px; margin: 6px 2px 0; }}
</style>
</head>
<body>
<main>
<header><h1>Victris Fleet Status</h1><span class="stamp">as of {at} UTC</span></header>
<div class="tiles">
  <div class="tile"><b>{up} of {total}</b><span>bots up</span></div>
  <div class="tile"><b>{games}</b><span>games in 24 hours</span></div>
  <div class="tile"><b>{alerts_day}</b><span>alerts in 24 hours</span></div>
  <div class="tile"><b>{corpus}</b><span>verified replays, +{corpus_day} today</span></div>
  <div class="tile"><b>{load:.1}</b><span>load average, batch work {batch}</span></div>
</div>

<h2>Bots</h2>
<div class="wrap"><table>
<tr><th>Bot</th><th>State</th><th>Release</th><th class="n">Version</th><th class="n">Rating</th><th class="n">Games, 24 h</th><th class="n">W&ndash;L&ndash;D</th><th class="n">Score</th><th class="n">Think p99</th></tr>
{bots}</table></div>
<p class="note">Score counts a draw as half a win; the range under it is the 95% interval. Think p99 is over the last 90 seconds of play.</p>

<h2>Experiments</h2>
<div class="wrap"><table>
<tr><th>Experiment</th><th>Arm</th><th class="n">Games</th><th class="n">W&ndash;L&ndash;D</th><th class="n">Score</th></tr>
{arms}</table></div>

<h2>Releases</h2>
<div class="wrap"><table>
<tr><th>Release</th><th>Status</th><th>Arena gate</th><th>Ladder evidence</th></tr>
{releases}</table></div>
<p class="note">Live is the build the flagship runs: {live}. The corpus holds {strong} games between bots rated 2400 and up; {queued} replays are waiting to be fetched.</p>

<h2>Alerts and pipeline log</h2>
<div class="wrap"><ul>{alerts}</ul></div>
</main>
</body>
</html>
"##,
        at = esc(&s.at.replace('T', " ").replace('Z', "")),
        total = s.bots.len(),
        alerts_day = s.alerts_day,
        corpus = s.corpus_ok,
        corpus_day = s.corpus_day,
        load = s.load,
        batch = if s.paused.is_some() { "paused" } else { "may run" },
        live = esc(&s.live),
        strong = s.corpus_strong,
        queued = s.corpus_queued,
    )
}
