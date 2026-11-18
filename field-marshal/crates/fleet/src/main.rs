//! fleet: runs the bots, plans experiments, keeps the results, ships releases, grows the corpus.
//!
//!   fleet status                                  every bot: running? which release? today's record
//!   fleet run <bot>                               the supervisor (launchd runs this)
//!   fleet gateway                                 one process holding the ladder connections of every bot with "gateway": true
//!   fleet install <bot> | watch | collector | online | autopilot | status | gateway      install a launchd agent
//!   fleet uninstall <bot> | watch | collector | online | autopilot | status | gateway
//!   fleet stop <bot> | resume <bot> | restart <bot>
//!   fleet deploy <bot> <release> [--version vN] [--force "<reason>"]
//!
//!   fleet stats [--by opponent|version|release|playbook|arm|arm-tier|arm-remote|remote|connection|map|seat|bot|cell] [--bot b]
//!               [--opponent o] [--version v] [--release r] [--experiment e] [--since 24h]
//!               [--min-opp-rating 2400] [--all]
//!   fleet compare <candidate> <baseline>          two sets of ladder games, each a release id or bot:version
//!   fleet ingest [bot ...]                        read marshal logs into the store
//!
//!   fleet plan list | show <name> | add <file.json> | pause <name> | resume <name> | stop <name>
//!   fleet plan show <name> [--since 90m|04:45|<epoch>]   arms, and each arm against control over the arm's own time
//!   fleet plan add-arm <name> <arm> '<settings json>' | stop-arm <name> <arm> [--reason text] | resume-arm <name> <arm> | rebalance <name>
//!   fleet epoch add <name> <time> [--note text] | list        named moments, usable wherever --since is
//!   fleet assign --bot b --opponent o --map m --seat n --match id     what the plan would hand out (writes the row)
//!
//!   fleet release list
//!   fleet release build [--ref HEAD] [--note text] [--no-tests]
//!   fleet release gate <id> [--against <id>] [--games 400] [--workers 2] [--s0 0.50] [--s1 0.56]
//!   fleet release canary <id> [--force "<reason>"]
//!   fleet release evidence <id> [--baseline <release | bot:version>] [--min-games 60] [--confidence 0.9]
//!   fleet release promote <id> [--force "<reason>"]
//!   fleet release mark <id> <status> --reason "<why>"
//!
//!   fleet collect import                          bring in replays the GitHub workflow fetched (corpus branch)
//!   fleet collect run | once [--max N] | index    the local collector (off: it disturbs the live bots)
//!   fleet corpus [--min-rating 2400] [--bot name] [--ours | --not-ours] [--since 24h] [--limit N] [--count]
//!   fleet watch                                   one health pass (launchd runs this every 5 minutes)
//!   fleet online [run]                            is each bot online on the ladder? `run` is the offline watchdog
//!   fleet autopilot [run]                         one step of the automatic pipeline; `run` keeps going
//!   fleet serve [--port 8787]                     the status page, live: built from the store on every request
//!   fleet report                                  write the status page to data/status.html
//!   fleet digest                                  the last 24 hours in a few lines (first line fits a notification)
//!   fleet ladder                                  fetch the ladder's standings into the store (one request)
//!   fleet alerts [N]                              the last N lines of the alert log

use fleet::collect::{self, Collector};
use fleet::config::Config;
use fleet::plan::PENDING_MS;
use fleet::release::{self, EvidenceOptions, GateOptions};
use fleet::stats::{table, Filter};
use fleet::store::{data_dir, default_db, now_ms, repo_root, MatchStart, Store};
use fleet::supervise::{self, bot_pid, label, launchd_loaded};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct Args {
    words: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

/// Flags that take no value.
const SWITCHES: [&str; 8] = ["--all", "--no-tests", "--ours", "--not-ours", "--count", "--experiments", "--json", "--rebuild"];

impl Args {
    fn parse(raw: &[String]) -> Args {
        let mut a = Args { words: Vec::new(), flags: Vec::new() };
        let mut it = raw.iter();
        while let Some(x) = it.next() {
            if x.starts_with("--") {
                if SWITCHES.contains(&x.as_str()) {
                    a.flags.push((x.clone(), None));
                } else {
                    a.flags.push((x.clone(), it.next().cloned()));
                }
            } else {
                a.words.push(x.clone());
            }
        }
        a
    }
    fn get(&self, flag: &str) -> Option<&str> {
        self.flags.iter().find(|(k, _)| k == flag).and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == flag)
    }
    fn num<T: std::str::FromStr>(&self, flag: &str) -> Result<Option<T>, String> {
        match self.get(flag) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| format!("{flag} needs a number, got '{v}'")),
        }
    }
    fn word(&self, k: usize, what: &str) -> Result<&str, String> {
        self.words.get(k).map(String::as_str).ok_or_else(|| format!("missing {what}; `fleet help` shows the commands"))
    }
}

/// "90m", "24h", "7d" before now, as a time in milliseconds.
fn since(text: &str) -> Result<i64, String> {
    let (n, unit) = text.split_at(text.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| format!("--since takes forms like 90m, 24h or 7d, got '{text}'"))?;
    let ms = match unit {
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("--since takes forms like 90m, 24h or 7d, got '{text}'")),
    };
    Ok(now_ms() - n * ms)
}

/// A moment: "90m", "24h" or "7d" before now; "04:45" today in UTC; a full
/// "2026-11-18T04:45:00Z"; or the name of an epoch in the store.
fn when(store: &Store, text: &str) -> Result<i64, String> {
    if let Ok(ms) = store.conn.query_row("SELECT ts_ms FROM epochs WHERE name = ?1", [text], |r| r.get::<_, i64>(0)) {
        return Ok(ms);
    }
    if let Some(ms) = collect::iso_ms(text) {
        return Ok(ms);
    }
    if let Some((h, m)) = text.split_once(':') {
        if let (Ok(h), Ok(m)) = (h.parse::<i64>(), m.parse::<i64>()) {
            let day = now_ms() - now_ms().rem_euclid(86_400_000);
            return Ok(day + (h * 60 + m) * 60_000);
        }
    }
    since(text).map_err(|_| format!("'{text}' is not a time: use 90m, 24h, 7d, 04:45 (UTC today), 2026-11-18T04:45:00Z or an epoch's name (`fleet epoch list`)"))
}

fn ago(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "never".into() };
    let s = (now_ms() - ms).max(0) / 1000;
    match s {
        0..=119 => format!("{s} s ago"),
        120..=7199 => format!("{} min ago", s / 60),
        7200..=172_799 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

fn e<T: std::fmt::Display>(x: T) -> String {
    x.to_string()
}

/// For a bot the fleet does not run: the release or version of its latest game.
fn last_seen(store: &Store, bot: &str, column: &str) -> Option<String> {
    assert!(column == "release" || column == "version");
    store.conn.query_row(&format!("SELECT {column} FROM matches WHERE bot = ?1 ORDER BY started_ms DESC LIMIT 1"), [bot], |r| r.get(0)).ok().flatten()
}

fn status(root: &Path, store: &Store, cfg: &Config) -> Result<(), String> {
    let _ = release::sync_flagship(root, store, cfg);
    println!("bot             role      agent    process   release        version  last game     24h record");
    for b in &cfg.bots {
        let st = store.bot_state(&b.name).map_err(e)?;
        let loaded = launchd_loaded(&label(&b.name));
        let pid = bot_pid(root, &b.name);
        let loaded = loaded || (b.gateway && launchd_loaded("com.victris.fleet-gateway"));
        let process = match (b.managed, pid, st.as_ref().and_then(|s| s.halted.clone())) {
            (false, _, _) => "(own)".to_string(),
            (true, _, Some(_)) => "HALTED".to_string(),
            (true, Some(p), _) => format!("pid {p}"),
            (true, None, _) => "down".to_string(),
        };
        let last: Option<i64> = store.conn.query_row("SELECT MAX(COALESCE(ended_ms, started_ms)) FROM matches WHERE bot = ?1", [&b.name], |r| r.get(0)).map_err(e)?;
        let day = store.records("all", &Filter { bot: Some(b.name.clone()), since_ms: Some(now_ms() - 86_400_000), ..Filter::default() }).map_err(e)?;
        let rec = day.first().map(|r| format!("{}W {}L {}D ({:.0}%)", r.record.wins, r.record.losses, r.record.draws, 100.0 * r.record.score())).unwrap_or_else(|| "no games".into());
        println!(
            "{:<15} {:<9} {:<8} {:<9} {:<14} {:<8} {:<13} {}",
            b.name,
            b.role,
            if loaded { "loaded" } else { "-" },
            process,
            st.as_ref().and_then(|s| s.release.clone()).or_else(|| last_seen(store, &b.name, "release")).unwrap_or_else(|| "-".into()),
            st.as_ref().and_then(|s| s.version.clone()).or_else(|| last_seen(store, &b.name, "version")).unwrap_or_else(|| "-".into()),
            ago(last),
            rec
        );
        if let Some(h) = st.and_then(|s| s.halted) {
            println!("    halted: {h}");
        }
    }
    let live = store.live_release().map_err(e)?.map(|r| r.id).unwrap_or_else(|| "none".into());
    let active: Vec<String> = store.experiments(true).map_err(e)?.into_iter().map(|x| x.name).collect();
    let (ok, queued): (i64, i64) = store.conn.query_row("SELECT COALESCE(SUM(status = 'ok'), 0), COALESCE(SUM(status = 'queued'), 0) FROM replays", [], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e)?;
    let last_replay: Option<i64> = store.conn.query_row("SELECT MAX(fetched_ms) FROM replays WHERE status = 'ok'", [], |r| r.get(0)).map_err(e)?;
    // The gateway's comms budget, when it is running.
    if let Some(g) = std::fs::read_to_string(supervise::run_dir(root).join("gateway.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        if now_ms() - g["ts_ms"].as_i64().unwrap_or(0) < 60_000 {
            let b = |k: &str| format!("{} of {}", g[k]["used"], g[k]["limit"]);
            println!(
                "\ngateway: {} bots; comms budget in use: connections {} a minute, queue requests {} a minute, matches {} a day; since start {} drops, {} idle turns sent for late engines",
                g["bots"].as_array().map(|a| a.len()).unwrap_or(0),
                b("connections"),
                b("queue_requests"),
                b("matches"),
                g["since_start"]["drops"],
                g["since_start"]["idle_turns"]
            );
        }
    }
    println!("\nlive release: {live}");
    println!("experiments running: {}", if active.is_empty() { "none".to_string() } else { active.join(", ") });
    println!(
        "replay corpus: {ok} verified, {queued} waiting, last added {}; collector agent {}; watch agent {}",
        ago(last_replay),
        if launchd_loaded("com.victris.fleet-collector") { "loaded" } else { "not loaded" },
        if launchd_loaded("com.victris.fleet-watch") { "loaded" } else { "not loaded" }
    );
    Ok(())
}

fn plan(store: &mut Store, a: &Args) -> Result<(), String> {
    match a.word(1, "a plan command (list, show, add, add-arm, stop-arm, resume-arm, rebalance, pause, resume, stop)")? {
        "list" => {
            for x in store.experiments(false).map_err(e)? {
                let done: i64 = store.conn.query_row("SELECT COUNT(*) FROM matches WHERE experiment_id = ?1 AND result IS NOT NULL AND voided = 0", [x.id], |r| r.get(0)).map_err(e)?;
                println!(
                    "{:<28} {:<7} {} arms x {} games, {} played; targets {}; bots {}",
                    x.name,
                    x.status,
                    x.arms.len(),
                    x.games_per_arm,
                    done,
                    x.opponents.join(", "),
                    x.bots.map(|b| b.join(", ")).unwrap_or_else(|| "every experimenting bot".into())
                );
            }
        }
        "show" => {
            let name = a.word(2, "the experiment's name")?;
            let x = store.experiment(name).map_err(e)?.ok_or_else(|| format!("no experiment named {name}"))?;
            println!("{} ({}), {} games per arm, targets {}", x.name, x.status, x.games_per_arm, x.opponents.join(", "));
            if let Some(n) = &x.note {
                println!("{n}");
            }
            for (arm, settings) in &x.arms {
                println!("  arm {arm}: {settings}");
            }
            let from = match a.get("--since").or(a.get("--epoch")) {
                Some(t) => Some(when(store, t)?),
                None => None,
            };
            if let Some(t) = from {
                println!("counting games started {} or later", ago(Some(t)));
            }
            let f = Filter { experiment: Some(name.to_string()), since_ms: from, ..Filter::default() };
            print!("\n{}", table("arm", &store.records("arm", &f).map_err(e)?));
            // Each arm against the control's games from that arm's own first game on, so an
            // arm that joined late is not compared with a control that played in other times.
            println!("\neach arm against control over the arm's own time");
            println!("{:<18} {:>5} {:>13}   {:>5} {:>13}   {:>8}  {}", "arm", "games", "record", "games", "control", "diff", "sure it is better");
            let mut names: Vec<String> = x.arms.iter().map(|(n, _)| n.clone()).chain(x.stopped.iter().map(|(n, _)| n.clone())).filter(|n| n != "control").collect();
            names.dedup();
            for arm in names {
                let first: Option<i64> = store
                    .conn
                    .query_row(
                        "SELECT MIN(started_ms) FROM matches WHERE experiment_id = ?1 AND arm = ?2 AND result IS NOT NULL AND started_ms >= ?3",
                        rusqlite::params![x.id, arm, from.unwrap_or(0)],
                        |r| r.get(0),
                    )
                    .map_err(e)?;
                let Some(t0) = first else { continue };
                let window = Filter { experiment: Some(name.to_string()), since_ms: Some(t0), ..Filter::default() };
                let rows = store.records("arm", &window).map_err(e)?;
                let rec = |n: &str| rows.iter().find(|r| r.key == n).map(|r| r.record).unwrap_or_default();
                let (ar, cr) = (rec(&arm), rec("control"));
                let (line, sure) = if ar.games() > 0 && cr.games() > 0 {
                    // Smoothed variances: a 6 and 0 record must not claim to be certain.
                    let smooth = |r: &fleet::stats::Record| {
                        let p = (r.wins as f64 + 0.5 * r.draws as f64 + 1.0) / (r.games() as f64 + 2.0);
                        p * (1.0 - p) / r.games() as f64
                    };
                    let se = (smooth(&ar) + smooth(&cr)).sqrt();
                    let diff = ar.score() - cr.score();
                    (format!("{:+.0} pts", 100.0 * diff), if se > 0.0 { format!("{:.0}%", 100.0 * fleet::stats::normal_cdf(diff / se)) } else { "-".into() })
                } else {
                    ("-".to_string(), "-".to_string())
                };
                println!(
                    "{:<18} {:>5} {:>13}   {:>5} {:>13}   {:>8}  {}",
                    arm,
                    ar.games(),
                    format!("{}W {}L {}D", ar.wins, ar.losses, ar.draws),
                    cr.games(),
                    format!("{}W {}L {}D", cr.wins, cr.losses, cr.draws),
                    line,
                    sure
                );
            }
            for (arm, why) in &x.stopped {
                println!("  arm {arm} was stopped: {why}");
            }
            print!("\n{}", table("arm by opponent strength", &store.records("arm-tier", &f).map_err(e)?));
            print!("\n{}", table("opponent", &store.records("opponent", &f).map_err(e)?));
            // Balance: games each arm has played in every cell it has met.
            let mut q = store
                .conn
                .prepare(
                    "SELECT opponent || ' / ' || map || ' / seat ' || COALESCE(seat, '?'), arm, COUNT(*) FROM matches
                     WHERE experiment_id = ?1 AND voided = 0 AND (result IS NOT NULL OR started_ms > ?2) GROUP BY 1, 2 ORDER BY 1, 2",
                )
                .map_err(e)?;
            let rows: Vec<(String, String, i64)> = q.query_map(rusqlite::params![x.id, now_ms() - PENDING_MS], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(e)?.filter_map(|r| r.ok()).collect();
            println!("\nbalance (games per arm in each opponent / map / seat cell)");
            let mut cell = String::new();
            let mut line = String::new();
            for (c, arm, n) in rows {
                if c != cell {
                    if !line.is_empty() {
                        println!("  {cell}: {line}");
                    }
                    cell = c;
                    line.clear();
                }
                line.push_str(&format!("{arm} {n}  "));
            }
            if !line.is_empty() {
                println!("  {cell}: {line}");
            }
        }
        "add" => {
            let path = a.word(2, "the experiment file")?;
            let v: Value = serde_json::from_str(&std::fs::read_to_string(path).map_err(|x| format!("{path}: {x}"))?).map_err(|x| format!("{path}: {x}"))?;
            let name = v.get("name").and_then(Value::as_str).ok_or("the file needs a \"name\"")?;
            let list = |k: &str| -> Option<Vec<String>> { v.get(k).and_then(Value::as_array).map(|x| x.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()) };
            let opponents = list("opponents").filter(|o| !o.is_empty()).ok_or("the file needs \"opponents\": a list of bot names (\"*\" matches any run of characters)")?;
            let arms: Vec<(String, Value)> = v.get("arms").and_then(Value::as_object).map(|o| o.iter().map(|(k, s)| (k.clone(), s.clone())).collect()).unwrap_or_default();
            if arms.len() < 2 {
                return Err("the file needs \"arms\": at least two, each a name with playbook settings ({} is the bot's own playbook)".into());
            }
            let games = v.get("games_per_arm").and_then(Value::as_i64).unwrap_or(100);
            let priority = v.get("priority").and_then(Value::as_i64).unwrap_or(0);
            let id = store.add_experiment(name, &opponents, list("bots").as_deref(), &arms, games, priority, v.get("note").and_then(Value::as_str)).map_err(|x| format!("cannot add {name}: {x}"))?;
            println!("experiment {name} added (id {id}): {} arms x {games} games against {}", arms.len(), opponents.join(", "));
        }
        "add-arm" => {
            let name = a.word(2, "the experiment's name")?;
            let arm = a.word(3, "the new arm's name")?;
            let settings: Value = serde_json::from_str(a.word(4, "the arm's playbook settings as JSON, e.g. '{\"slam_risk\": 8}'")?).map_err(|x| format!("the settings are not JSON: {x}"))?;
            if !settings.is_object() {
                return Err("the settings must be a JSON object".into());
            }
            match store.add_arm(name, arm, &settings) {
                Ok(1) => println!("arm {arm} added to {name}; it is handed out first until it has caught up"),
                Ok(_) => return Err(format!("no experiment named {name}")),
                Err(x) => return Err(format!("cannot add the arm: {x}")),
            }
        }
        "resume-arm" => {
            let name = a.word(2, "the experiment's name")?;
            let arm = a.word(3, "the arm's name")?;
            let x = store.experiment(name).map_err(e)?.ok_or_else(|| format!("no experiment named {name}"))?;
            if store.resume_arm(x.id, arm).map_err(e)? == 0 {
                return Err(format!("{name} has no stopped arm named {arm}"));
            }
            store.event(None, "arm_resumed", &json!({ "experiment": name, "arm": arm }));
            println!("arm {arm} of {name} runs again; every running arm gets an equal share from now");
        }
        "rebalance" => {
            let name = a.word(2, "the experiment's name")?;
            if store.rebalance(name).map_err(e)? == 0 {
                return Err(format!("no experiment named {name}"));
            }
            println!("{name}: every running arm gets an equal share from now");
        }
        "stop-arm" => {
            let name = a.word(2, "the experiment's name")?;
            let arm = a.word(3, "the arm's name")?;
            let x = store.experiment(name).map_err(e)?.ok_or_else(|| format!("no experiment named {name}"))?;
            let why = a.get("--reason").unwrap_or("stopped by hand");
            if store.stop_arm(x.id, arm, why).map_err(e)? == 0 {
                return Err(format!("{name} has no running arm named {arm}"));
            }
            store.event(None, "arm_stopped", &json!({ "experiment": name, "arm": arm, "why": why }));
            println!("arm {arm} of {name} is stopped ({why}); its games stay in the record");
        }
        cmd @ ("pause" | "resume" | "stop") => {
            let name = a.word(2, "the experiment's name")?;
            let status = match cmd {
                "pause" => "paused",
                "resume" => "active",
                _ => "done",
            };
            if store.set_experiment_status(name, status).map_err(e)? == 0 {
                return Err(format!("no experiment named {name}"));
            }
            println!("{name} is now {status}");
        }
        other => return Err(format!("unknown plan command {other}")),
    }
    Ok(())
}

fn release_cmd(root: &Path, store: &Store, cfg: &Config, a: &Args) -> Result<(), String> {
    match a.word(1, "a release command (list, build, gate, canary, evidence, promote, mark)")? {
        "list" => {
            println!("release                    status    built         arena gate                               ladder evidence");
            for r in store.releases().map_err(e)? {
                let gate = r.gate.as_ref().map(|g| if g["games"].is_null() { format!("{} ({})", g["verdict"].as_str().unwrap_or("?"), g["against"].as_str().or(g["note"].as_str()).unwrap_or("")) } else { format!("{} {:.0}% of {} vs {}", g["verdict"].as_str().unwrap_or("?"), 100.0 * g["score"].as_f64().unwrap_or(0.0), g["games"], g["against"].as_str().unwrap_or("?")) }).unwrap_or_else(|| "-".into());
                let ev = r.evidence.as_ref().map(|v| format!("{} ({} games, {:+.1} points, {:.0}% sure)", v["verdict"].as_str().unwrap_or("?"), v["candidate"]["games"], 100.0 * v["score_difference"].as_f64().unwrap_or(0.0), 100.0 * v["confidence_better"].as_f64().unwrap_or(0.0))).unwrap_or_else(|| "-".into());
                println!("{:<26} {:<9} {:<13} {:<40} {}", r.id, r.status, ago(Some(r.created_ms)), gate, ev);
                if let Some(n) = r.note {
                    println!("    {n}");
                }
            }
        }
        "build" => {
            let r = release::build(root, store, a.get("--ref").unwrap_or("HEAD"), a.get("--note"), !a.has("--no-tests"))?;
            println!("release {} built from {} at {}", r.id, &r.git_sha[..7], r.path);
        }
        "gate" => {
            let id = a.word(2, "the release id")?;
            let d = GateOptions::default();
            let o = GateOptions {
                against: a.get("--against").map(str::to_string),
                max_games: a.num("--games")?.unwrap_or(d.max_games),
                workers: a.num("--workers")?.unwrap_or(d.workers),
                seed: a.num("--seed")?.unwrap_or(d.seed),
                s0: a.num("--s0")?.unwrap_or(d.s0),
                s1: a.num("--s1")?.unwrap_or(d.s1),
            };
            let g = release::gate(root, store, id, &o)?;
            println!(
                "arena gate {}: {} scored {:.0}% over {} games against {} ({}W {}L {}D); {} is now '{}'",
                g["verdict"].as_str().unwrap_or("?"),
                id,
                100.0 * g["score"].as_f64().unwrap_or(0.0),
                g["games"],
                g["against"].as_str().unwrap_or("?"),
                g["wins"],
                g["losses"],
                g["draws"],
                id,
                store.release(id).map_err(e)?.map(|r| r.status).unwrap_or_default()
            );
        }
        "canary" => {
            let id = a.word(2, "the release id")?;
            for (bot, version) in release::canary(root, store, cfg, id, a.get("--force"))? {
                println!("{bot} now runs {id} as {version}");
            }
        }
        "evidence" => {
            let id = a.word(2, "the release id")?;
            let d = EvidenceOptions::default();
            let o = EvidenceOptions {
                baseline: a.get("--baseline").map(str::to_string),
                min_games: a.num("--min-games")?.unwrap_or(d.min_games),
                min_shared_opponents: a.num("--min-opponents")?.unwrap_or(d.min_shared_opponents),
                confidence: a.num("--confidence")?.unwrap_or(d.confidence),
            };
            let v = release::evidence(store, id, &o)?;
            if a.has("--json") {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
            }
            println!(
                "ladder evidence {}: {} has {} games ({:.0}%), baseline {} has {} ({:.0}%); over {} shared opponents it scores {:+.1} points, {:.0}% sure it is better (needs {:.0}%)",
                v["verdict"].as_str().unwrap_or("?"),
                id,
                v["candidate"]["games"],
                100.0 * v["candidate"]["score"].as_f64().unwrap_or(0.0),
                v["baseline"].as_str().unwrap_or("?"),
                v["baseline_record"]["games"],
                100.0 * v["baseline_record"]["score"].as_f64().unwrap_or(0.0),
                v["shared_opponents"],
                100.0 * v["score_difference"].as_f64().unwrap_or(0.0),
                100.0 * v["confidence_better"].as_f64().unwrap_or(0.0),
                100.0 * v["needed"].as_f64().unwrap_or(0.0)
            );
            if let Some(w) = v["waiting_for"].as_array().filter(|w| !w.is_empty()) {
                println!("still waiting for: {}", w.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; "));
            }
        }
        "promote" => {
            let id = a.word(2, "the release id")?;
            let out = release::promote(root, store, cfg, id, a.get("--force"))?;
            println!("{id} is now the live release");
            for (bot, version) in out {
                println!("{bot} now runs {id} as {version}");
            }
        }
        "mark" => {
            let id = a.word(2, "the release id")?;
            let status = a.word(3, "the status")?;
            if !release::ORDER.contains(&status) {
                return Err(format!("status must be one of {}", release::ORDER.join(", ")));
            }
            let reason = a.get("--reason").ok_or("--reason \"<why>\" is required: marking by hand skips the pipeline, so it is logged")?;
            if store.release(id).map_err(e)?.is_none() {
                return Err(format!("no release {id}"));
            }
            if status == "live" {
                if let Some(p) = store.live_release().map_err(e)?.filter(|p| p.id != id) {
                    store.set_release_status(&p.id, "retired").map_err(e)?;
                }
            }
            store.set_release_status(id, status).map_err(e)?;
            store.event(None, "release_marked", &json!({ "release": id, "status": status, "reason": reason }));
            println!("{id} is now '{status}' ({reason})");
        }
        other => return Err(format!("unknown release command {other}")),
    }
    Ok(())
}

/// One health pass: bring in results the bots did not write themselves, and restart a bot
/// that is alive but has gone silent.
fn watch(root: &Path, store: &Store, cfg: &Config) -> Result<(), String> {
    for b in &cfg.bots {
        let dir = root.join("logs").join(&b.name);
        if dir.is_dir() {
            match fleet::ingest::ingest_logs(store, &dir, &b.name, &|o| cfg.is_ours(o)) {
                Ok(n) if n.started + n.finished > 0 => println!("{}: {} games added from logs, {} completed", b.name, n.started, n.finished),
                Ok(_) => {}
                Err(x) => eprintln!("{}: cannot read logs: {x}", b.name),
            }
        }
        if !b.managed {
            continue;
        }
        let halted = store.bot_state(&b.name).map_err(e)?.and_then(|s| s.halted);
        if let (Some(pid), None) = (bot_pid(root, &b.name), halted) {
            // A bot on the ladder writes a log line every tick, and queues within seconds
            // of a game ending. Twenty silent minutes means it is stuck.
            let newest = std::fs::read_dir(&dir).ok().and_then(|d| d.filter_map(|f| f.ok()?.metadata().ok()?.modified().ok()).max());
            let started: Option<i64> = store.conn.query_row("SELECT MAX(started_ms) FROM runs WHERE bot = ?1", [&b.name], |r| r.get(0)).map_err(e)?;
            let silent = newest.and_then(|t| t.elapsed().ok()).map(|d| d > Duration::from_secs(1200)).unwrap_or(true);
            let old_enough = started.map(|s| now_ms() - s > 1_200_000).unwrap_or(false);
            if silent && old_enough {
                println!("{}: pid {pid} has written nothing for 20 minutes; killing it so the supervisor restarts it", b.name);
                store.event(Some(&b.name), "watchdog_kill", &json!({ "pid": pid }));
                supervise::signal(pid, libc::SIGKILL);
            }
        } else if !launchd_loaded(&label(&b.name)) {
            println!("{}: managed, but its launchd agent is not loaded (`fleet install {}`)", b.name, b.name);
        }
    }
    Ok(())
}

/// Agents that are not bots.
const SERVICES: [&str; 6] = ["watch", "collector", "online", "autopilot", "status", "gateway"];

/// Once a day: a snapshot of the store, and old tick logs of the bots we run compressed.
/// Nothing is deleted except our own snapshots beyond the newest seven.
fn housekeeping(root: &Path, store: &Store, cfg: &Config) {
    let day = fleet::online::utc_stamp()[..10].to_string();
    let dir = data_dir(root).join("backup");
    let snapshot = dir.join(format!("fleet-{day}.db"));
    if snapshot.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    // VACUUM INTO writes a consistent copy while the bots keep writing.
    match store.conn.execute("VACUUM INTO ?1", [snapshot.display().to_string()]) {
        Ok(_) => println!("store snapshot written to {}", snapshot.display()),
        Err(x) => {
            eprintln!("store snapshot failed: {x}");
            fleet::online::alert(root, "ALERT", None, &format!("the nightly store snapshot failed: {x}"));
            return;
        }
    }
    let mut old: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().filter_map(|f| f.ok()).map(|f| f.path()).filter(|p| p.file_name().map(|n| n.to_string_lossy().starts_with("fleet-")).unwrap_or(false)).collect();
    old.sort();
    while old.len() > 7 {
        let _ = std::fs::remove_file(old.remove(0));
    }
    // Tick logs are about 50 KB a game. Two days on, everything in them is in the store.
    for b in cfg.bots.iter().filter(|b| b.managed) {
        let logs = root.join("logs").join(&b.name);
        for f in std::fs::read_dir(&logs).into_iter().flatten().filter_map(|f| f.ok()) {
            let path = f.path();
            let aged = f.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).map(|d| d > Duration::from_secs(2 * 86_400)).unwrap_or(false);
            if aged && path.extension().map(|x| x == "jsonl").unwrap_or(false) {
                let ok = std::process::Command::new("gzip").arg(&path).status().map(|s| s.success()).unwrap_or(false);
                println!("{}: {}", path.display(), if ok { "compressed" } else { "could not be compressed" });
            }
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn agent_spec(root: &Path, what: &str) -> (String, Vec<String>, std::path::PathBuf, Option<u32>) {
    let bin = data_dir(root).join("bin").join("fleet").display().to_string();
    match what {
        "watch" => ("com.victris.fleet-watch".into(), vec![bin, "watch".into()], root.join("logs").join("fleet-watch.log"), Some(300)),
        "collector" => ("com.victris.fleet-collector".into(), vec![bin, "collect".into(), "run".into()], root.join("logs").join("fleet-collector.log"), None),
        "autopilot" => ("com.victris.fleet-autopilot".into(), vec![bin, "autopilot".into(), "run".into()], root.join("logs").join("fleet-autopilot.log"), None),
        "gateway" => ("com.victris.fleet-gateway".into(), vec![bin, "gateway".into()], root.join("logs").join("fleet-gateway.log"), None),
        "status" => ("com.victris.fleet-status".into(), vec![bin, "serve".into()], root.join("logs").join("fleet-status.log"), None),
        "online" => ("com.victris.fleet-online".into(), vec![bin, "online".into(), "run".into()], root.join("logs").join("fleet-online.log"), None),
        bot => (label(bot), vec![bin, "run".into(), bot.to_string()], root.join("logs").join(bot).join("run.log"), None),
    }
}

fn main_inner() -> Result<i32, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let a = Args::parse(&raw);
    let root = repo_root();
    let cmd = a.words.first().map(String::as_str).unwrap_or("help");
    if cmd == "help" || a.has("--help") {
        // The usage is the comment at the top of this file.
        for line in include_str!("main.rs").lines().take_while(|l| l.starts_with("//!")) {
            println!("{}", line.trim_start_matches("//!").strip_prefix(' ').unwrap_or(""));
        }
        return Ok(0);
    }
    if cmd == "run" {
        return Ok(supervise::run(&root, a.word(1, "the bot's name")?));
    }
    let mut store = Store::open(&default_db(&root), Duration::from_secs(10)).map_err(|x| format!("cannot open the store: {x}"))?;
    let cfg = Config::load(&root)?;
    let known = |bot: &str| -> Result<(), String> { cfg.bot(bot).map(|_| ()).ok_or_else(|| format!("{bot} is not in fleet/bots.json")) };
    match cmd {
        "status" => status(&root, &store, &cfg)?,
        "install" => {
            let what = a.word(1, "a bot's name, `watch` or `collector`")?;
            if !SERVICES.contains(&what) {
                known(what)?;
                if !cfg.bot(what).unwrap().managed {
                    return Err(format!("{what} is not managed by the fleet (managed is false in fleet/bots.json)"));
                }
                if !root.join("bots").join(what).is_file() {
                    return Err(format!("no token file bots/{what}: register the bot first"));
                }
            }
            supervise::install_self(&root)?;
            let (lbl, args, log, interval) = agent_spec(&root, what);
            // A bot's supervisor takes as long to stop as its match takes to finish, and
            // launchd will not load the new agent while the old one is still there. So the
            // bot is asked to finish first, held down for the moment of the swap, and
            // released once the new agent is in.
            let mut held = false;
            if !SERVICES.contains(&what) && launchd_loaded(&lbl) {
                if let Some(pid) = bot_pid(&root, what) {
                    let was_halted = store.bot_state(what).map_err(e)?.and_then(|s| s.halted).is_some();
                    if !was_halted {
                        store.set_halted(what, Some("being reinstalled")).map_err(e)?;
                        held = true;
                    }
                    supervise::signal(pid, libc::SIGTERM);
                    println!("waiting for {what} (pid {pid}) to finish its match");
                    let until = std::time::Instant::now() + Duration::from_secs(900);
                    while supervise::alive(pid) && std::time::Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            if let Some(dir) = log.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let installed = supervise::install_agent(&lbl, &supervise::plist(&lbl, &root, &args, &log, interval, what == "collector" || what == "autopilot"));
            if held {
                store.set_halted(what, None).map_err(e)?;
            }
            installed?;
            println!("{lbl} installed and loaded; log {}", log.display());
        }
        "uninstall" => {
            let what = a.word(1, "a bot's name, `watch` or `collector`")?;
            let (lbl, ..) = agent_spec(&root, what);
            if !SERVICES.contains(&what) {
                // Let the match finish before launchd takes the supervisor away.
                if let Some(pid) = supervise::restart_bot(&root, what) {
                    println!("waiting for {what} (pid {pid}) to finish its match");
                    let _ = store.set_halted(what, Some("being uninstalled"));
                    while supervise::alive(pid) {
                        std::thread::sleep(Duration::from_millis(250));
                    }
                }
            }
            supervise::remove_agent(&lbl)?;
            if !SERVICES.contains(&what) {
                let _ = store.set_halted(what, None);
            }
            println!("{lbl} removed");
        }
        "stop" => {
            let bot = a.word(1, "the bot's name")?;
            known(bot)?;
            store.set_halted(bot, Some(a.get("--reason").unwrap_or("stopped by hand"))).map_err(e)?;
            if cfg.bot(bot).map(|b| b.gateway).unwrap_or(false) {
                println!("{bot}: the gateway takes it off the ladder after its current match; it stays down until `fleet resume {bot}`");
                return Ok(0);
            }
            match supervise::restart_bot(&root, bot) {
                Some(pid) => println!("{bot}: pid {pid} will finish its match and stay down until `fleet resume {bot}`"),
                None => println!("{bot}: was not running; it stays down until `fleet resume {bot}`"),
            }
        }
        "resume" => {
            let bot = a.word(1, "the bot's name")?;
            known(bot)?;
            store.set_halted(bot, None).map_err(e)?;
            println!("{bot}: resumed; its supervisor starts it within 15 seconds{}", if launchd_loaded(&label(bot)) { "" } else { " once its agent is installed (`fleet install`)" });
        }
        "restart" => {
            let bot = a.word(1, "the bot's name")?;
            if cfg.bot(bot).map(|b| b.gateway).unwrap_or(false) {
                return Err(format!("{bot} runs behind the gateway, which restarts its engine by itself; use `fleet deploy` to change what it runs"));
            }
            match supervise::restart_bot(&root, bot) {
                Some(pid) => println!("{bot}: pid {pid} will finish its match, then its supervisor starts it again"),
                None => println!("{bot}: not running"),
            }
        }
        "deploy" => {
            let bot = a.word(1, "the bot's name")?;
            let id = a.word(2, "the release id")?;
            let version = release::deploy(&root, &store, &cfg, bot, id, a.get("--version"), a.get("--force"))?;
            println!("{bot} is set to run {id} as {version}");
        }
        "stats" => {
            let f = Filter {
                bot: a.get("--bot").map(str::to_string),
                opponent: a.get("--opponent").map(str::to_string),
                version: a.get("--version").map(str::to_string),
                release: a.get("--release").map(str::to_string),
                experiment: a.get("--experiment").map(str::to_string),
                since_ms: a.get("--since").or(a.get("--epoch")).map(|t| when(&store, t)).transpose()?,
                no_ours: !a.has("--all"),
                min_opponent_rating: a.num("--min-opp-rating")?,
            };
            let by = a.get("--by").unwrap_or("version");
            if fleet::stats::group_column(by).is_none() {
                return Err(format!("--by takes opponent, version, release, playbook, arm, arm-tier, arm-remote, remote, connection, map, seat, bot, cell or all; got '{by}'"));
            }
            print!("{}", table(by, &store.records(by, &f).map_err(e)?));
        }
        "compare" => print!("{}", release::compare(&store, a.word(1, "the candidate (release id or bot:version)")?, a.word(2, "the baseline (release id or bot:version)")?)?),
        "ingest" => {
            let bots: Vec<String> = if a.words.len() > 1 { a.words[1..].to_vec() } else { cfg.names() };
            for bot in bots {
                let dir = root.join("logs").join(&bot);
                if !dir.is_dir() {
                    continue;
                }
                let n = fleet::ingest::ingest_logs(&store, &dir, &bot, &|o| cfg.is_ours(o)).map_err(e)?;
                println!("{bot}: {} log files, {} games added, {} completed", n.files, n.started, n.finished);
            }
        }
        "plan" => plan(&mut store, &a)?,
        "assign" => {
            let need = |f: &str| a.get(f).map(str::to_string).ok_or(format!("{f} is required"));
            let m = MatchStart {
                match_id: need("--match")?,
                bot: need("--bot")?,
                version: a.get("--version").unwrap_or("test").to_string(),
                release: None,
                opponent: need("--opponent")?,
                map: need("--map")?,
                seat: a.num("--seat")?,
                seed: None,
            };
            let ours = cfg.is_ours(&m.opponent);
            match store.assign(&m, "base", true, ours).map_err(e)? {
                Some(x) => println!("{} {}", x.playbook_name(), x.settings),
                None => println!("no experiment: the bot plays its own playbook"),
            }
        }
        "release" => release_cmd(&root, &store, &cfg, &a)?,
        "collect" => match a.word(1, "run, once, index or import")? {
            "index" => {
                let (added, bad) = collect::index_corpus(&root, &store, &cfg.names()).map_err(e)?;
                println!("{added} replays on disk verified and indexed, {bad} failed verification");
            }
            "import" => {
                let (copied, verified, bad) = collect::import_branch(&root, &store, &cfg.names())?;
                println!("{copied} replays brought in from the corpus branch; {verified} verified and indexed, {bad} failed verification");
            }
            mode @ ("run" | "once") => {
                unsafe {
                    libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
                    libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
                }
                let mut opt = collect::Options::default();
                if let Some(g) = a.num::<u64>("--gap")? {
                    opt.gap = Duration::from_secs(g.max(20));
                }
                if mode == "once" {
                    opt.max_downloads = Some(a.num("--max")?.unwrap_or(20));
                }
                let mut c = Collector::new(&root, &store, cfg.names(), opt);
                c.run(&|| STOP.load(Ordering::Relaxed));
            }
            other => return Err(format!("unknown collect command {other}")),
        },
        "corpus" => {
            let q = collect::Query {
                min_rating: a.num("--min-rating")?,
                bot: a.get("--bot").map(str::to_string),
                ours: if a.has("--ours") { Some(true) } else if a.has("--not-ours") { Some(false) } else { None },
                since_ms: a.get("--since").map(since).transpose()?,
                limit: a.num("--limit")?,
            };
            let paths = collect::corpus(&root, &store, &q).map_err(e)?;
            if a.has("--count") {
                println!("{}", paths.len());
            } else {
                for p in paths {
                    println!("{}", p.display());
                }
            }
        }
        "watch" => {
            watch(&root, &store, &cfg)?;
            housekeeping(&root, &store, &cfg);
            // The standings, when the snapshot in the store has grown old.
            if cfg.ladder_refresh_minutes > 0 {
                let last: i64 = store.conn.query_row("SELECT COALESCE(MAX(updated_ms), 0) FROM ladder", [], |r| r.get(0)).unwrap_or(0);
                if now_ms() - last >= cfg.ladder_refresh_minutes as i64 * 60_000 {
                    match fleet::online::refresh_ladder(&store, &cfg.names()) {
                        Ok(n) => println!("standings of {n} bots stored"),
                        Err(x) => eprintln!("standings not refreshed: {x}"),
                    }
                }
            }
            // Replays fetched off this network arrive through the repository's corpus branch.
            match if cfg.corpus_branch { collect::import_branch(&root, &store, &cfg.names()) } else { Ok((0, 0, 0)) } {
                Ok((copied, verified, bad)) if copied + verified + bad > 0 => println!("corpus branch: {copied} replays brought in, {verified} verified, {bad} failed verification"),
                Ok(_) => {}
                Err(x) => eprintln!("corpus branch not imported: {x}"),
            }
            let _ = std::fs::write(data_dir(&root).join("status.html"), fleet::report::html(&fleet::report::snapshot(&root, &store, &cfg)));
        }
        "online" => {
            if a.words.get(1).map(String::as_str) == Some("run") {
                unsafe {
                    libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
                    libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
                }
                drop(store);
                fleet::online::run(&root, &|| STOP.load(Ordering::Relaxed));
            } else {
                println!(
                    "load average {:.1} (guard at {:.0}); batch work {}",
                    fleet::online::load_average(),
                    cfg.guard_load,
                    match std::fs::read_to_string(fleet::online::pause_file(&root)) {
                        Ok(why) => format!("PAUSED since {}", why.trim()),
                        Err(_) => "may run".to_string(),
                    }
                );
                for b in cfg.bots.iter().filter(|b| b.expect_online) {
                    if let Some(p99) = fleet::online::recent_think_p99_ms(&root, &b.name) {
                        println!("{:<15} think time p99 over the last 90 s: {p99:.0} ms (guard at {:.0})", b.name, cfg.guard_think_p99_ms);
                    }
                }
                for b in &cfg.bots {
                    let state = match fleet::online::is_online(&b.name) {
                        Ok(true) => "online".to_string(),
                        Ok(false) => "OFFLINE".to_string(),
                        Err(x) => format!("unknown ({x})"),
                    };
                    println!("{:<15} {:<8} {}", b.name, state, if b.expect_online { "watched" } else { "not watched" });
                }
            }
        }
        "autopilot" => {
            drop(store);
            let mut pilot = fleet::autopilot::Autopilot::default();
            if a.words.get(1).map(String::as_str) == Some("run") {
                unsafe {
                    libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
                    libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
                }
                pilot.run(&root, &|| STOP.load(Ordering::Relaxed));
            } else {
                pilot.step(&root)?;
            }
        }
        "report" => {
            let snap = fleet::report::snapshot(&root, &store, &cfg);
            let path = data_dir(&root).join("status.html");
            std::fs::write(&path, fleet::report::html(&snap)).map_err(e)?;
            println!("{}", path.display());
            // The same numbers as tables, for the hosted page.
            let dir = data_dir(&root).join("status");
            fleet::report::write_csv(&snap, &dir).map_err(e)?;
            std::fs::write(dir.join("state.json"), fleet::report::state_json(&snap).to_string()).map_err(e)?;
            println!("{}", dir.display());
        }
        "gateway" => {
            unsafe {
                libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
                libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            }
            drop(store);
            fleet::gateway::run(&root, std::sync::Arc::new(|| STOP.load(Ordering::Relaxed)));
        }
        "serve" => {
            unsafe {
                libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
                libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            }
            drop(store);
            let port: u16 = a.num("--port")?.unwrap_or(8787);
            fleet::report::serve(&root, port, &|| STOP.load(Ordering::Relaxed)).map_err(|x| format!("cannot serve on port {port}: {x}"))?;
        }
        "digest" => print!("{}", fleet::report::digest(&fleet::report::snapshot(&root, &store, &cfg))),
        "epoch" => match a.word(1, "add or list")? {
            "add" => {
                let name = a.word(2, "the epoch's name")?;
                let at = when(&store, a.word(3, "the time")?)?;
                store.conn.execute("INSERT OR REPLACE INTO epochs (name, ts_ms, note) VALUES (?1, ?2, ?3)", rusqlite::params![name, at, a.get("--note")]).map_err(e)?;
                println!("epoch {name} set to {}", ago(Some(at)));
            }
            "list" => {
                let mut q = store.conn.prepare("SELECT name, ts_ms, COALESCE(note, '') FROM epochs ORDER BY ts_ms").map_err(e)?;
                let rows: Vec<(String, i64, String)> = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(e)?.filter_map(|r| r.ok()).collect();
                for (name, ts, note) in rows {
                    println!("{name:<20} {:<12} {note}", ago(Some(ts)));
                }
            }
            other => return Err(format!("unknown epoch command {other}")),
        },
        "ladder" => {
            let n = fleet::online::refresh_ladder(&store, &cfg.names())?;
            println!("standings of {n} bots stored");
        }
        "alerts" => {
            let n: usize = a.words.get(1).and_then(|n| n.parse().ok()).unwrap_or(20);
            let text = std::fs::read_to_string(fleet::online::alert_log(&root)).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            for l in &lines[lines.len().saturating_sub(n)..] {
                println!("{l}");
            }
        }
        other => return Err(format!("unknown command {other}; `fleet help` shows the commands")),
    }
    Ok(0)
}

fn main() {
    // Output piped into `head` must end the program quietly, not with a panic.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match main_inner() {
        Ok(code) => std::process::exit(code),
        Err(msg) => {
            eprintln!("fleet: {msg}");
            std::process::exit(2);
        }
    }
}
