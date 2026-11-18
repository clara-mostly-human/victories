//! The state of the fleet in one place, as a page (`data/status.html`, rewritten every five
//! minutes by the health pass) and as a short text digest (`fleet digest`).

use crate::config::Config;
use crate::online::{alert_log, last_turn_age_s, load_average, pause_file, recent_think_p99_ms, utc_stamp};
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
    /// place on the ladder, when this version is ranked
    pub rank: Option<i64>,
    pub rating_day: f64,
    pub last_game_ms: Option<i64>,
    pub think_p99_ms: Option<f64>,
    /// seconds since the bot last answered a turn (from its log)
    pub last_turn_s: Option<f64>,
    /// the gateway's verdict on this bot and its level, when the bot is behind the gateway
    pub gateway: Option<(String, String)>,
    /// its socket to the ladder: the last ping, and the median time on the wire per turn (ms)
    pub ping_ms: Option<f64>,
    pub wire_ms: Option<f64>,
}

/// One finished game, as the bot and the ladder snapshot know it.
pub struct GameLine {
    pub ended_ms: i64,
    pub bot: String,
    pub version: String,
    pub result: String,
    pub reason: String,
    pub opponent: String,
    pub opponent_rating: Option<f64>,
    pub opponent_rank: Option<i64>,
    pub map: String,
    pub seat: Option<i64>,
    pub ticks: Option<i64>,
    pub rating_change: Option<f64>,
    pub rating_after: Option<f64>,
    pub dealt: i64,
    pub taken: i64,
    pub playbook: String,
    pub charge_p50_ms: Option<i64>,
    pub lost_turns: Option<i64>,
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
    /// the latest finished games, newest first (of one bot when the page is filtered)
    pub games: Vec<GameLine>,
    pub only_bot: Option<String>,
    pub ladder_age_ms: Option<i64>,
    /// arms split by how strong the opponent is
    pub arm_tiers: Vec<(String, Record)>,
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
    /// the gateway's health verdict, when the gateway is running
    pub health: Option<GatewayHealth>,
}

/// The gateway's verdict, as it last wrote it (`data/run/gateway.json`).
#[derive(Clone, Debug, Default)]
pub struct GatewayHealth {
    /// "ok" or "degraded"
    pub word: String,
    pub problems: Vec<String>,
    /// how old the gateway's report is
    pub age_s: i64,
    /// each bot behind the gateway: its verdict and level ("ok", "warn", "bad")
    pub bots: std::collections::HashMap<String, (String, String)>,
    /// each bot's socket: the last ping and the median wire time per turn, in ms
    pub sockets: std::collections::HashMap<String, (Option<f64>, Option<f64>)>,
}

pub fn gateway_health(root: &Path) -> Option<GatewayHealth> {
    let text = std::fs::read_to_string(crate::store::data_dir(root).join("run").join("gateway.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let mut h = GatewayHealth {
        word: v.get("verdict")?.as_str()?.to_string(),
        problems: v.get("problems").and_then(|p| p.as_array()).map(|p| p.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        age_s: (now_ms() - v.get("ts_ms").and_then(|t| t.as_i64()).unwrap_or(0)) / 1000,
        bots: Default::default(),
        sockets: Default::default(),
    };
    for (name, b) in v.get("health").and_then(|x| x.as_object())? {
        h.bots.insert(name.clone(), (b.get("verdict").and_then(|x| x.as_str()).unwrap_or("?").to_string(), b.get("level").and_then(|x| x.as_str()).unwrap_or("bad").to_string()));
        let num = |k: &str| b.get("socket").and_then(|s| s.get(k)).and_then(|x| x.as_f64());
        h.sockets.insert(name.clone(), (num("ping_ms"), num("wire_ms")));
    }
    Some(h)
}

fn one<T: rusqlite::types::FromSql>(store: &Store, sql: &str, p: impl rusqlite::Params) -> Option<T> {
    store.conn.query_row(sql, p, |r| r.get::<_, Option<T>>(0)).ok().flatten()
}

pub fn snapshot(root: &Path, store: &Store, cfg: &Config) -> Snapshot {
    snapshot_for(root, store, cfg, None)
}

/// The snapshot; `only_bot` narrows the list of games to one of our bots.
pub fn snapshot_for(root: &Path, store: &Store, cfg: &Config, only_bot: Option<&str>) -> Snapshot {
    let only_bot = only_bot.filter(|b| cfg.is_ours(b)).map(str::to_string);
    let games: Vec<GameLine> = {
        let mut q = store
            .conn
            .prepare(
                "SELECT COALESCE(m.ended_ms, m.started_ms), m.bot, COALESCE(m.version, ''), m.result, COALESCE(m.reason, ''), COALESCE(m.opponent, '?'),
                        m.opponent_rating, (SELECT rank FROM ladder l WHERE l.name = m.opponent), COALESCE(m.map, ''), m.seat, m.final_tick,
                        m.rating_change, m.rating_after, m.detail, COALESCE(m.playbook, ''), m.charge_p50_ms, m.lost_turns
                 FROM matches m WHERE m.result IS NOT NULL AND (?1 IS NULL OR m.bot = ?1) ORDER BY 1 DESC LIMIT 60",
            )
            .ok();
        match q.as_mut() {
            None => Vec::new(),
            Some(q) => q
                .query_map([only_bot.as_deref()], |r| {
                    let detail: Option<String> = r.get(13)?;
                    let damage = detail.and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok()).and_then(|d| d.get("damage").cloned());
                    let sum = |prefix: &str| -> i64 { damage.as_ref().and_then(|d| d.as_object()).map(|o| o.iter().filter(|(k, _)| k.starts_with(prefix)).filter_map(|(_, v)| v.as_i64()).sum()).unwrap_or(0) };
                    Ok(GameLine {
                        ended_ms: r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        bot: r.get(1)?,
                        version: r.get(2)?,
                        result: r.get(3)?,
                        reason: r.get(4)?,
                        opponent: r.get(5)?,
                        opponent_rating: r.get(6)?,
                        opponent_rank: r.get(7)?,
                        map: r.get(8)?,
                        seat: r.get(9)?,
                        ticks: r.get(10)?,
                        rating_change: r.get(11)?,
                        rating_after: r.get(12)?,
                        dealt: sum("dealt:"),
                        taken: sum("taken:"),
                        playbook: r.get(14)?,
                        charge_p50_ms: r.get(15)?,
                        lost_turns: r.get(16)?,
                    })
                })
                .map(|rows| rows.filter_map(|g| g.ok()).collect())
                .unwrap_or_default(),
        }
    };
    let ladder_age_ms: Option<i64> = one::<i64>(store, "SELECT MAX(updated_ms) FROM ladder", []).map(|t| now_ms() - t);
    let arm_tiers: Vec<(String, Record)> = store
        .experiments(true)
        .unwrap_or_default()
        .iter()
        .flat_map(|e| store.records("arm-tier", &Filter { experiment: Some(e.name.clone()), ..Filter::default() }).unwrap_or_default())
        .map(|r| (r.key, r.record))
        .collect();
    snapshot_inner(root, store, cfg, games, only_bot, ladder_age_ms, arm_tiers)
}

fn snapshot_inner(root: &Path, store: &Store, cfg: &Config, games: Vec<GameLine>, only_bot: Option<String>, ladder_age_ms: Option<i64>, arm_tiers: Vec<(String, Record)>) -> Snapshot {
    let health = gateway_health(root);
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
        // Our rating after the latest game: carried forward from the ladder's own figure by
        // the change each game brought (the ladder tells a bot only the change).
        let rating: Option<f64> = store
            .conn
            .query_row(
                "SELECT COALESCE(rating_after, rating_before + COALESCE(rating_change, 0)) FROM matches
                 WHERE bot = ?1 AND result IS NOT NULL AND (rating_after IS NOT NULL OR rating_before IS NOT NULL)
                   AND version IS (SELECT version FROM matches WHERE bot = ?1 ORDER BY started_ms DESC LIMIT 1)
                 ORDER BY COALESCE(ended_ms, started_ms) DESC LIMIT 1",
                [&b.name],
                |r| r.get(0),
            )
            .ok();
        // The ladder ranks one version of a bot; the place only counts while that is the
        // version playing now.
        let rank: Option<i64> = store
            .conn
            .query_row(
                "SELECT rank FROM ladder WHERE name = ?1 AND version IS (SELECT version FROM matches WHERE bot = ?1 ORDER BY started_ms DESC LIMIT 1)",
                [&b.name],
                |r| r.get(0),
            )
            .ok();
        // The change over 24 hours, within the version playing now (every version starts afresh).
        let version_change: f64 = store
            .conn
            .query_row(
                "SELECT COALESCE(SUM(rating_change), 0) FROM matches WHERE bot = ?1 AND result IS NOT NULL AND started_ms >= ?2
                   AND version IS (SELECT version FROM matches WHERE bot = ?1 ORDER BY started_ms DESC LIMIT 1)",
                rusqlite::params![b.name, day],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
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
            rank,
            rating_day: version_change,
            last_game_ms: one(store, "SELECT MAX(COALESCE(ended_ms, started_ms)) FROM matches WHERE bot = ?1", [&b.name]),
            // The last 90 seconds of turns when the bot is in a match; between matches, the
            // last finished game's figure, so the column is never empty for a bot that plays.
            think_p99_ms: recent_think_p99_ms(root, &b.name).or_else(|| {
                store
                    .conn
                    .query_row("SELECT think_p99_us / 1000.0 FROM matches WHERE bot = ?1 AND result IS NOT NULL AND think_p99_us IS NOT NULL ORDER BY started_ms DESC LIMIT 1", [&b.name], |r| r.get(0))
                    .ok()
            }),
            last_turn_s: last_turn_age_s(root, &b.name),
            gateway: health.as_ref().filter(|h| h.age_s <= 15).and_then(|h| h.bots.get(&b.name).cloned()),
            ping_ms: health.as_ref().filter(|h| h.age_s <= 15).and_then(|h| h.sockets.get(&b.name).and_then(|s| s.0)),
            wire_ms: health.as_ref().filter(|h| h.age_s <= 15).and_then(|h| h.sockets.get(&b.name).and_then(|s| s.1)),
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
        games,
        only_bot,
        ladder_age_ms,
        arm_tiers,
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
        health,
    }
}

/// `HH:MM:SS` in UTC, for readers whose browser does not run the page's script.
fn utc_time(ms: i64) -> String {
    let s = (ms / 1000).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// What the bot is doing, and how to colour it. "playing" means it answered a turn within
/// the last 90 seconds; a bot that is up but has been given no match for longer says so,
/// with how long.
fn state(b: &BotLine) -> (String, &'static str) {
    // A bot behind the gateway: the gateway's verdict is the verdict.
    if let Some((verdict, level)) = &b.gateway {
        return (verdict.clone(), match level.as_str() {
            "ok" => "good",
            "warn" => "warn",
            _ => "bad",
        });
    }
    let stale = b.last_game_ms.map(|t| now_ms() - t > 900_000).unwrap_or(true);
    match (b.halted.is_some(), b.running, stale) {
        (true, _, _) => ("halted".into(), "bad"),
        (_, Some(false), _) => ("down".into(), "bad"),
        (_, _, true) => ("quiet".into(), "warn"),
        _ => match b.last_turn_s {
            Some(s) if s <= 90.0 => ("playing".into(), "good"),
            Some(s) if s < 3600.0 => (format!("no match for {:.0} min", (s / 60.0).max(2.0)), "warn"),
            _ => ("no match".into(), "warn"),
        },
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

/// The one verdict for the whole fleet: is every bot playing or waiting for a match, and
/// if not, which and why. From the gateway for the bots behind it, from the bots' own
/// logs for the rest.
pub fn fleet_health(s: &Snapshot) -> (bool, Vec<String>) {
    let mut problems: Vec<String> = Vec::new();
    match &s.health {
        Some(h) if h.age_s > 15 => problems.push(format!("the gateway has not reported for {} s: it is down or stuck", h.age_s)),
        Some(h) => problems.extend(h.problems.iter().cloned()),
        None => {}
    }
    for b in s.bots.iter().filter(|b| b.gateway.is_none()) {
        let (word, level) = state(b);
        if level != "good" && !problems.iter().any(|p| p.starts_with(&format!("{}:", b.name))) {
            problems.push(format!("{}: {word}", b.name));
        }
    }
    (problems.is_empty(), problems)
}

pub fn html(s: &Snapshot) -> String {
    let (healthy, problems) = fleet_health(s);
    let behind = s.bots.iter().filter(|b| b.gateway.is_some()).count();
    let banner = if healthy {
        format!("<div class=\"banner good\"><b>Healthy.</b> Every bot is playing or waiting for a match. {behind} of {} are behind the gateway.</div>", s.bots.len())
    } else {
        format!("<div class=\"banner bad\"><b>Not healthy.</b> {}</div>", problems.iter().map(|p| esc(p)).collect::<Vec<_>>().join("<br>"))
    };
    let mut bots = String::new();
    for b in &s.bots {
        let (word, class) = state(b);
        let (lo, hi) = b.day.interval();
        bots.push_str(&format!(
            "<tr><td><strong><a href=\"/?bot={0}\">{0}</a></strong><span class=\"sub\">{1}</span></td><td><span class=\"pill {class}\">{word}</span></td><td>{2}<span class=\"sub\">{3}</span></td><td class=\"n\">{4}</td><td class=\"n\">{5}<span class=\"sub\">{rank}{6:+.0} on this version</span></td><td class=\"n\">{7}<span class=\"sub\">{8} last hour</span></td><td class=\"n\">{9}&ndash;{10}&ndash;{11}</td><td class=\"n\">{12}<span class=\"sub\">{13}</span></td><td class=\"n\">{14}</td><td class=\"n\">{15}</td><td class=\"n\">{16}</td></tr>\n",
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
            b.ping_ms.map(|p| format!("{p:.0} ms")).unwrap_or_else(|| "&ndash;".into()),
            b.wire_ms.map(|p| format!("{p:.0} ms")).unwrap_or_else(|| "&ndash;".into()),
            rank = b.rank.map(|r| format!("#{r}, ")).unwrap_or_default(),
        ));
    }
    let mut game_rows = String::new();
    for g in &s.games {
        let class = match g.result.as_str() {
            "win" => "good",
            "loss" => "bad",
            _ => "warn",
        };
        let strength = match (g.opponent_rating, g.opponent_rank) {
            (Some(r), Some(k)) => format!("rated {r:.0}, #{k}"),
            (Some(r), None) => format!("rated {r:.0}"),
            _ => "not on the last standings".to_string(),
        };
        let rating = match (g.rating_change, g.rating_after) {
            (Some(c), Some(a)) => format!("{a:.0}<span class=\"sub\">{c:+.0}</span>"),
            (Some(c), None) => format!("{c:+.0}"),
            _ => "&ndash;".into(),
        };
        let conn = match (g.charge_p50_ms, g.lost_turns) {
            (Some(c), Some(l)) if l > 0 => format!("{c} ms<span class=\"sub\">{l} turns lost</span>"),
            (Some(c), _) => format!("{c} ms"),
            _ => "&ndash;".into(),
        };
        game_rows.push_str(&format!(
            "<tr><td class=\"n\"><time data-ms=\"{}\">{}</time></td><td><a href=\"/?bot={bot}\">{bot}</a><span class=\"sub\">{}</span></td><td><span class=\"pill {class}\">{}</span><span class=\"sub\">{}</span></td><td><strong>{}</strong><span class=\"sub\">{strength}</span></td><td>{}<span class=\"sub\">seat {}, {} ticks</span></td><td class=\"n\">{rating}</td><td class=\"n\">{}<span class=\"sub\">took {}</span></td><td>{}</td><td class=\"n\">{conn}</td></tr>\n",
            g.ended_ms,
            esc(&utc_time(g.ended_ms)),
            esc(&g.version),
            esc(&g.result),
            esc(&g.reason.replace('_', " ")),
            esc(&g.opponent),
            esc(&g.map),
            g.seat.map(|x| x.to_string()).unwrap_or_else(|| "?".into()),
            g.ticks.map(|x| x.to_string()).unwrap_or_else(|| "?".into()),
            g.dealt,
            g.taken,
            esc(&g.playbook),
            bot = esc(&g.bot),
        ));
    }
    if game_rows.is_empty() {
        game_rows = "<tr><td colspan=\"9\" class=\"sub\">No finished game yet.</td></tr>".into();
    }
    let mut tiers = String::new();
    for (key, r) in &s.arm_tiers {
        let (lo, hi) = r.interval();
        tiers.push_str(&format!(
            "<tr><td>{}</td><td class=\"n\">{}</td><td class=\"n\">{}&ndash;{}&ndash;{}</td><td class=\"n\">{:.0}%<span class=\"sub\">{:.0} to {:.0}%</span></td></tr>\n",
            esc(key),
            r.games(),
            r.wins,
            r.losses,
            r.draws,
            100.0 * r.score(),
            100.0 * lo,
            100.0 * hi
        ));
    }
    let games_title = match &s.only_bot {
        Some(b) => format!("Latest games of {} <a class=\"all\" href=\"/\">show all bots</a>", esc(b)),
        None => "Latest games".to_string(),
    };
    let standings = match s.ladder_age_ms {
        Some(ms) => format!("Opponent ratings and ranks are from the ladder's standings as fetched {} minutes ago.", ms / 60_000),
        None => "Opponent ratings and ranks appear once the ladder's standings have been fetched.".to_string(),
    };
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
th, td {{ padding: 10px 14px; text-align: left; border-bottom: 1px solid var(--line); vertical-align: top; }}
th {{ font-size: 12px; font-weight: 600; color: var(--muted); white-space: nowrap; }}
tr:last-child td {{ border-bottom: 0; }}
.n {{ text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }}
th.n {{ text-align: right; }}
.sub {{ display: block; color: var(--muted); font-size: 12px; font-weight: 400; }}
.pill {{ display: inline-block; padding: 4px 12px; border-radius: 999px; font-size: 12px; line-height: 1.3; font-weight: 600; background: var(--line); color: var(--ink); }}
.pill.good {{ background: var(--good-bg); color: var(--good); }}
.pill.warn {{ background: var(--warn-bg); color: var(--warn); }}
.pill.bad {{ background: var(--bad-bg); color: var(--bad); }}
.banner {{ margin: 0 0 16px; padding: 12px 16px; border-radius: 10px; font-size: 14px; line-height: 1.5; }}
.banner.good {{ background: var(--good-bg); color: var(--good); }}
.banner.bad {{ background: var(--bad-bg); color: var(--bad); }}
.bar {{ height: 4px; background: var(--line); border-radius: 2px; margin-top: 5px; min-width: 90px; }}
.bar i {{ display: block; height: 4px; background: var(--accent); border-radius: 2px; }}
ul {{ list-style: none; margin: 0; padding: 12px 16px; font: 12px/1.6 ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere; }}
.note {{ color: var(--muted); font-size: 13px; margin: 6px 2px 0; }}
a {{ color: var(--accent); text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
h2 a.all {{ font-weight: 400; text-transform: none; letter-spacing: 0; margin-left: 10px; }}
</style>
</head>
<body>
<main>
<header><h1>Victris Fleet Status</h1><span class="stamp">as of {at} UTC</span></header>
{banner}
<div class="tiles">
  <div class="tile"><b>{up} of {total}</b><span>bots up</span></div>
  <div class="tile"><b>{games}</b><span>games in 24 hours</span></div>
  <div class="tile"><b>{alerts_day}</b><span>alerts in 24 hours</span></div>
  <div class="tile"><b>{corpus}</b><span>verified replays, +{corpus_day} today</span></div>
  <div class="tile"><b>{load:.1}</b><span>load average, batch work {batch}</span></div>
</div>

<h2>Bots</h2>
<div class="wrap"><table>
<tr><th>Bot</th><th>State</th><th>Release</th><th class="n">Version</th><th class="n">Rating</th><th class="n">Games, 24 h</th><th class="n">W&ndash;L&ndash;D</th><th class="n">Score</th><th class="n">Think p99</th><th class="n">Ping</th><th class="n">Wire per turn</th></tr>
{bots}</table></div>
<p class="note">Score counts a draw as half a win; the range under it is the 95% interval. Think p99 is over the last 90 seconds of play, or the last finished game when the bot is between matches. State is the gateway's verdict for a bot behind the gateway: it says "playing" only while turns are being answered. Ping is the last round trip on the bot's socket to the ladder (measured at connect and every 20 seconds); wire per turn is the median of what the ladder charged for a turn less the time the bot took, over the current or last match. A good socket is about 40 to 65 ms ping and 70 ms on the wire; a slow one about 150 ms and 300.</p>

<h2>{games_title}</h2>
<div class="wrap"><table>
<tr><th class="n">Ended</th><th>Bot</th><th>Result</th><th>Opponent</th><th>Map</th><th class="n">Rating after</th><th class="n">Damage dealt</th><th>Playbook or arm</th><th class="n">Charged per turn</th></tr>
{game_rows}</table></div>
<p class="note">Each bot writes a game here the moment it ends. Rating after is the bot's rating carried forward by the change the ladder reported. {standings} Charged per turn is the median of what the ladder billed for a turn: our thinking plus the trip there and back.</p>

<h2>Experiments</h2>
<div class="wrap"><table>
<tr><th>Experiment</th><th>Arm</th><th class="n">Games</th><th class="n">W&ndash;L&ndash;D</th><th class="n">Score</th></tr>
{arms}</table></div>

<p class="note">The same arms by how strong the opponent is:</p>
<div class="wrap"><table>
<tr><th>Arm and opponents</th><th class="n">Games</th><th class="n">W&ndash;L&ndash;D</th><th class="n">Score</th></tr>
{tiers}</table></div>

<h2>Releases</h2>
<div class="wrap"><table>
<tr><th>Release</th><th>Status</th><th>Arena gate</th><th>Ladder evidence</th></tr>
{releases}</table></div>
<p class="note">Live is the build the flagship runs: {live}. The corpus holds {strong} games between bots rated 2400 and up; {queued} replays are waiting to be fetched.</p>

<h2>Alerts and pipeline log</h2>
<div class="wrap"><ul>{alerts}</ul></div>
</main>
<script>
// Times are stored in UTC; show them in the reader's own time.
for (const t of document.querySelectorAll('time[data-ms]')) {{
  const d = new Date(Number(t.dataset.ms));
  if (!isNaN(d)) t.textContent = d.toLocaleTimeString([], {{ hour: '2-digit', minute: '2-digit', second: '2-digit' }});
}}
</script>
</body>
</html>
"##,
        games_title = games_title,
        game_rows = game_rows,
        standings = standings,
        tiers = if tiers.is_empty() { "<tr><td colspan=\"4\" class=\"sub\">No experiment is running.</td></tr>".to_string() } else { tiers },
        at = esc(&s.at.replace('T', " ").replace('Z', "")),
        banner = banner,
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

fn csv_cell(t: &str) -> String {
    if t.contains(',') || t.contains('"') || t.contains('\n') {
        format!("\"{}\"", t.replace('"', "\"\""))
    } else {
        t.to_string()
    }
}

fn csv(header: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut out = header.join(",");
    out.push('\n');
    for r in rows {
        out.push_str(&r.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

/// The snapshot as plain tables (`summary.csv`, `bots.csv`, `arms.csv`, `releases.csv`,
/// `alerts.csv`), for the hosted status page, which draws from attached files.
pub fn write_csv(s: &Snapshot, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let up = s.bots.iter().filter(|b| state(b).1 != "bad").count();
    let games: i64 = s.bots.iter().map(|b| b.day.games()).sum();
    std::fs::write(
        dir.join("summary.csv"),
        csv(
            &["as_of", "bots_up", "bots", "games_24h", "alerts_24h", "replays", "replays_24h", "replays_2400", "replays_waiting", "load", "batch", "live_release"],
            vec![vec![
                s.at.clone(),
                up.to_string(),
                s.bots.len().to_string(),
                games.to_string(),
                s.alerts_day.to_string(),
                s.corpus_ok.to_string(),
                s.corpus_day.to_string(),
                s.corpus_strong.to_string(),
                s.corpus_queued.to_string(),
                format!("{:.1}", s.load),
                if s.paused.is_some() { "paused".into() } else { "may run".into() },
                s.live.clone(),
            ]],
        ),
    )?;
    std::fs::write(
        dir.join("bots.csv"),
        csv(
            &["bot", "role", "state", "release", "version", "rating", "rating_change_24h", "games_24h", "games_last_hour", "wins", "losses", "draws", "score_pct", "think_p99_ms"],
            s.bots
                .iter()
                .map(|b| {
                    vec![
                        b.name.clone(),
                        b.role.clone(),
                        state(b).0.to_string(),
                        b.release.clone(),
                        b.version.clone(),
                        b.rating.map(|r| format!("{r:.0}")).unwrap_or_default(),
                        format!("{:.0}", b.rating_day),
                        b.day.games().to_string(),
                        b.hour_games.to_string(),
                        b.day.wins.to_string(),
                        b.day.losses.to_string(),
                        b.day.draws.to_string(),
                        if b.day.games() > 0 { format!("{:.0}", 100.0 * b.day.score()) } else { String::new() },
                        b.think_p99_ms.map(|p| format!("{p:.0}")).unwrap_or_default(),
                    ]
                })
                .collect(),
        ),
    )?;
    std::fs::write(
        dir.join("arms.csv"),
        csv(
            &["experiment", "arm", "games", "target", "wins", "losses", "draws", "score_pct", "low_pct", "high_pct"],
            s.arms
                .iter()
                .map(|a| {
                    let (lo, hi) = a.record.interval();
                    vec![
                        a.experiment.clone(),
                        a.arm.clone(),
                        a.record.games().to_string(),
                        a.target.to_string(),
                        a.record.wins.to_string(),
                        a.record.losses.to_string(),
                        a.record.draws.to_string(),
                        if a.record.games() > 0 { format!("{:.0}", 100.0 * a.record.score()) } else { String::new() },
                        if a.record.games() > 0 { format!("{:.0}", 100.0 * lo) } else { String::new() },
                        if a.record.games() > 0 { format!("{:.0}", 100.0 * hi) } else { String::new() },
                    ]
                })
                .collect(),
        ),
    )?;
    std::fs::write(
        dir.join("releases.csv"),
        csv(&["release", "status", "arena_gate", "ladder_evidence", "note"], s.releases.iter().map(|r| vec![r.id.clone(), r.status.clone(), r.gate.clone(), r.evidence.clone(), r.note.clone()]).collect()),
    )?;
    std::fs::write(dir.join("alerts.csv"), csv(&["n", "line"], s.alerts.iter().enumerate().map(|(i, l)| vec![(i + 1).to_string(), l.clone()]).collect()))?;
    Ok(())
}

/// The snapshot as one JSON document: the hosted status page reads it from its store.
pub fn state_json(s: &Snapshot) -> serde_json::Value {
    let up = s.bots.iter().filter(|b| state(b).1 != "bad").count();
    let games: i64 = s.bots.iter().map(|b| b.day.games()).sum();
    let pct = |r: &Record| if r.games() > 0 { serde_json::json!((100.0 * r.score()).round()) } else { serde_json::Value::Null };
    serde_json::json!({
        "as_of": s.at,
        "summary": [{
            "as_of": s.at, "bots_up": up, "bots": s.bots.len(), "games_24h": games, "alerts_24h": s.alerts_day,
            "replays": s.corpus_ok, "replays_24h": s.corpus_day, "replays_2400": s.corpus_strong, "replays_waiting": s.corpus_queued,
            "load": format!("{:.1}", s.load), "batch": if s.paused.is_some() { "paused" } else { "may run" }, "live_release": s.live,
            "health": if fleet_health(s).0 { "healthy" } else { "NOT HEALTHY" }, "problems": fleet_health(s).1.join("; "),
        }],
        "bots": s.bots.iter().map(|b| serde_json::json!({
            "bot": b.name, "role": b.role, "state": state(b).0, "release": b.release, "version": b.version,
            "rating": b.rating.map(|r| r.round()), "rating_change_24h": b.rating_day.round(), "games_24h": b.day.games(), "games_last_hour": b.hour_games,
            "wins": b.day.wins, "losses": b.day.losses, "draws": b.day.draws, "score_pct": pct(&b.day), "think_p99_ms": b.think_p99_ms.map(|p| p.round()),
            "last_turn_s": b.last_turn_s.map(|t| t.round()), "ping_ms": b.ping_ms.map(|p| p.round()), "wire_ms": b.wire_ms.map(|p| p.round()),
        })).collect::<Vec<_>>(),
        "arms": s.arms.iter().map(|a| {
            let (lo, hi) = a.record.interval();
            serde_json::json!({
                "experiment": a.experiment, "arm": a.arm, "games": a.record.games(), "target": a.target,
                "wins": a.record.wins, "losses": a.record.losses, "draws": a.record.draws, "score_pct": pct(&a.record),
                "low_pct": if a.record.games() > 0 { serde_json::json!((100.0 * lo).round()) } else { serde_json::Value::Null },
                "high_pct": if a.record.games() > 0 { serde_json::json!((100.0 * hi).round()) } else { serde_json::Value::Null },
            })
        }).collect::<Vec<_>>(),
        "releases": s.releases.iter().map(|r| serde_json::json!({ "release": r.id, "status": r.status, "arena_gate": r.gate, "ladder_evidence": r.evidence, "note": r.note })).collect::<Vec<_>>(),
    })
}

/// A page served straight from the store: built for each request, so it is never older
/// than the last finished game, and it reloads itself every 20 seconds.
pub fn serve(root: &Path, port: u16, stop: &dyn Fn() -> bool) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    eprintln!("{} status page on port {port}", utc_stamp());
    let mut last_backfill = std::time::Instant::now() - std::time::Duration::from_secs(60);
    while !stop() {
        let (mut stream, _) = match listener.accept() {
            Ok(x) => x,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Err(_) => continue,
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
        let mut buf = [0u8; 2048];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or("/").to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.clone(), String::new()),
        };
        // `?bot=<name>` narrows the games to one of our bots; anything else is ignored.
        let only: Option<String> = query.split('&').find_map(|kv| kv.strip_prefix("bot=")).filter(|b| b.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).map(str::to_string);
        // Only two things are served, both read-only views of the store. Nothing is taken
        // from the request but its path.
        let built = (|| -> Result<(String, &'static str), String> {
            let cfg = Config::load(root)?;
            let store = Store::open(&crate::store::default_db(root), std::time::Duration::from_secs(2)).map_err(|e| e.to_string())?;
            if last_backfill.elapsed() >= std::time::Duration::from_secs(15) {
                last_backfill = std::time::Instant::now();
                let _ = store.backfill_ratings(&cfg.names());
            }
            let snap = snapshot_for(root, &store, &cfg, only.as_deref());
            Ok(match path.as_str() {
                "/" | "/index.html" | "/status.html" => (html(&snap).replacen("<meta charset=\"utf-8\">", "<meta charset=\"utf-8\">\n<meta http-equiv=\"refresh\" content=\"20\">", 1), "text/html; charset=utf-8"),
                "/state.json" => (state_json(&snap).to_string(), "application/json"),
                _ => return Err("not found".into()),
            })
        })();
        let (status, body, kind) = match built {
            Ok((body, kind)) => ("200 OK", body, kind),
            Err(e) if e == "not found" => ("404 Not Found", "not found\n".to_string(), "text/plain"),
            Err(e) => ("503 Service Unavailable", format!("the status page could not be built: {e}\n"), "text/plain"),
        };
        let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(body.as_bytes());
    }
    Ok(())
}
