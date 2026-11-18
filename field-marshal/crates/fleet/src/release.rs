//! The release pipeline. A release is a frozen build of one commit: the `marshal` and
//! `arena` binaries plus the playbooks file, in `data/releases/<id>/`. Live bots only ever
//! run releases, so rebuilding the working tree never changes a running bot.
//!
//! The path of a release:
//!
//!   built -> gated -> canary -> proven -> live -> retired
//!        \-> rejected
//!
//! - `build`: export the commit, build it in its own directory, run the tests.
//! - `gate`: the candidate plays the live release build against build (`fleet/duel.mjs`,
//!   the official simulator as referee) until a sequential test says it is better or not.
//! - `canary`: deploy to the scout bots, on the ladder, under a new version label.
//! - `evidence`: compare its ladder results with the live release, opponent by opponent.
//! - `promote`: deploy to the flagship. Refused unless the evidence passed.
//!
//! Every deploy is a hot swap: the store is changed, the bot gets SIGTERM, finishes its
//! match and exits, and its supervisor starts the new release.

use crate::config::Config;
use crate::stats::{paired, sprt, Filter, Record, Verdict};
use crate::store::{data_dir, now_ms, Release, Store};
use crate::supervise::restart_bot;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Statuses in the order a release moves through them.
pub const ORDER: [&str; 7] = ["rejected", "built", "gated", "canary", "proven", "live", "retired"];

fn rank(status: &str) -> usize {
    ORDER.iter().position(|s| *s == status).unwrap_or(0)
}

fn sh(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(25).collect();
        return Err(format!("{what} failed:\n{}", tail.into_iter().rev().collect::<Vec<_>>().join("\n")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
}

/// Builds a release from a commit. Uncommitted work in the checkout is not part of it.
pub fn build(root: &Path, store: &Store, git_ref: &str, note: Option<&str>, tests: bool) -> Result<Release, String> {
    let sha = sh(Command::new("git").current_dir(root).args(["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")]), "git rev-parse")?;
    let short = &sha[..7.min(sha.len())];
    if let Some(r) = store.releases().map_err(|e| e.to_string())?.into_iter().find(|r| r.git_sha == sha) {
        if Path::new(&r.path).join("marshal").is_file() {
            eprintln!("commit {short} is already release {}", r.id);
            return Ok(r);
        }
    }
    let build_dir = data_dir(root).join("build");
    let src = build_dir.join(format!("src-{short}"));
    let _ = std::fs::remove_dir_all(&src);
    std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
    eprintln!("exporting {short}");
    sh(
        Command::new("sh").current_dir(root).arg("-c").arg(format!("git archive {sha} field-marshal | tar -x -C '{}'", src.display())),
        "git archive",
    )?;
    let ws = src.join("field-marshal");
    let target = build_dir.join("target");
    eprintln!("building");
    sh(Command::new("cargo").current_dir(&ws).env("CARGO_TARGET_DIR", &target).args(["build", "--release", "-p", "marshal"]), "cargo build")?;
    if tests {
        eprintln!("running the tests");
        sh(Command::new("cargo").current_dir(&ws).env("CARGO_TARGET_DIR", &target).args(["test", "--release", "--workspace"]), "cargo test")?;
    }
    let seq = store.next_release_seq().map_err(|e| e.to_string())?;
    let id = format!("r{seq}-{short}");
    let dir = data_dir(root).join("releases").join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for bin in ["marshal", "arena"] {
        let from = target.join("release").join(bin);
        if from.is_file() {
            std::fs::copy(&from, dir.join(bin)).map_err(|e| format!("cannot copy {bin}: {e}"))?;
        } else if bin == "marshal" {
            return Err("the build produced no marshal binary".into());
        }
    }
    copy_dir(&ws.join("playbooks"), &dir.join("playbooks")).map_err(|e| format!("cannot copy playbooks: {e}"))?;
    let rustc = sh(Command::new("rustc").arg("--version"), "rustc").unwrap_or_default();
    let meta = json!({
        "id": id, "git_sha": sha, "built_ms": now_ms(), "rustc": rustc, "tests_run": tests,
        // Older commits have a marshal that does not know the fleet flags.
        "fleet_hook": ws.join("crates/bot/src/fleet_hook.rs").is_file(),
    });
    std::fs::write(dir.join("release.json"), serde_json::to_string_pretty(&meta).unwrap()).map_err(|e| e.to_string())?;
    store.add_release(&id, seq, &sha, &dir.display().to_string(), note).map_err(|e| e.to_string())?;
    store.event(None, "release_built", &meta);
    let _ = std::fs::remove_dir_all(&src);
    store.release(&id).map_err(|e| e.to_string())?.ok_or_else(|| "release not stored".to_string())
}

fn get(store: &Store, id: &str) -> Result<Release, String> {
    store.release(id).map_err(|e| e.to_string())?.ok_or_else(|| format!("no release {id}; `fleet release list` shows them"))
}

pub struct GateOptions {
    pub against: Option<String>,
    pub max_games: u32,
    pub workers: u32,
    pub seed: u32,
    /// the candidate must be shown to score `s1` rather than `s0` against the baseline
    pub s0: f64,
    pub s1: f64,
}

impl Default for GateOptions {
    fn default() -> Self {
        GateOptions { against: None, max_games: 400, workers: 2, seed: 1, s0: 0.50, s1: 0.56 }
    }
}

/// The arena gate: candidate against baseline, build against build, until decided.
pub fn gate(root: &Path, store: &Store, id: &str, o: &GateOptions) -> Result<Value, String> {
    let cand = get(store, id)?;
    let base = match &o.against {
        Some(b) => get(store, b)?,
        None => store.live_release().map_err(|e| e.to_string())?.ok_or("no release is live yet: name the baseline with --against <release>, or `fleet release mark <id> live` the first one")?,
    };
    if base.id == cand.id {
        return Err("a release cannot be gated against itself".into());
    }
    let playbooks = |r: &Release| Path::new(&r.path).join("playbooks").join("default.json");
    let mut child = Command::new("node")
        .current_dir(root)
        .arg(root.join("fleet").join("duel.mjs"))
        .arg("--a").arg(Path::new(&cand.path).join("marshal"))
        .arg("--a-playbooks").arg(playbooks(&cand))
        .arg("--b").arg(Path::new(&base.path).join("marshal"))
        .arg("--b-playbooks").arg(playbooks(&base))
        .arg("--games").arg(o.max_games.to_string())
        .arg("--workers").arg(o.workers.to_string())
        .arg("--seed").arg(o.seed.to_string())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start node fleet/duel.mjs: {e}"))?;
    let mut rec = Record::default();
    let (mut timeouts, mut base_timeouts, mut max_ms, mut ticks) = (0i64, 0i64, 0f64, 0i64);
    let mut verdict = Verdict::Undecided;
    let mut llr = 0.0;
    let mut error: Option<String> = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            error = Some(e.to_string());
            break;
        }
        let Some(result) = v.get("result").and_then(Value::as_str) else { continue };
        rec.add(result);
        timeouts += v.get("a_timeouts").and_then(Value::as_i64).unwrap_or(0);
        base_timeouts += v.get("b_timeouts").and_then(Value::as_i64).unwrap_or(0);
        max_ms = max_ms.max(v.get("a_ms").and_then(|m| m.get("max")).and_then(Value::as_f64).unwrap_or(0.0));
        ticks += v.get("ticks").and_then(Value::as_i64).unwrap_or(0);
        (llr, verdict) = sprt(&rec, o.s0, o.s1);
        if rec.games() % 10 == 0 || verdict != Verdict::Undecided {
            eprintln!("{} games: {}W {}L {}D, score {:.0}%, evidence {:+.2} (decides at +/-2.94)", rec.games(), rec.wins, rec.losses, rec.draws, 100.0 * rec.score(), llr);
        }
        // Only decide on whole pairs, so both seats of a seed are always counted.
        if verdict != Verdict::Undecided && rec.games() >= 20 && rec.games() % 2 == 0 {
            break;
        }
        verdict = Verdict::Undecided;
    }
    let _ = child.kill();
    let _ = child.wait();
    if let Some(e) = error {
        return Err(format!("the arena could not run: {e}"));
    }
    if rec.games() == 0 {
        return Err("the arena produced no games".into());
    }
    let (lo, hi) = rec.interval();
    let word = match verdict {
        Verdict::Pass => "pass",
        Verdict::Fail => "fail",
        Verdict::Undecided => "undecided",
    };
    let report = json!({
        "verdict": word, "against": base.id, "games": rec.games(), "wins": rec.wins, "losses": rec.losses, "draws": rec.draws,
        "score": rec.score(), "interval": [lo, hi], "llr": llr, "s0": o.s0, "s1": o.s1,
        "candidate_timeouts": timeouts, "baseline_timeouts": base_timeouts, "candidate_max_ms": max_ms,
        "mean_ticks": ticks as f64 / rec.games() as f64, "ts_ms": now_ms(),
    });
    store.set_release_json(id, "gate", &report).map_err(|e| e.to_string())?;
    // A release already further along keeps its status; a fresh one moves.
    if cand.status == "built" || cand.status == "rejected" || cand.status == "gated" {
        let status = match verdict {
            Verdict::Pass => "gated",
            Verdict::Fail => "rejected",
            Verdict::Undecided => "built",
        };
        store.set_release_status(id, status).map_err(|e| e.to_string())?;
    }
    store.event(None, "release_gate", &json!({ "release": id, "report": report }));
    Ok(report)
}

/// Deploys a release to a bot and swaps it in. Returns the ladder version label used.
pub fn deploy(root: &Path, store: &Store, cfg: &Config, bot: &str, id: &str, version: Option<&str>, force: Option<&str>) -> Result<String, String> {
    let b = cfg.bot(bot).ok_or_else(|| format!("{bot} is not in fleet/bots.json"))?;
    if !b.managed {
        return Err(format!("{bot} is not managed by the fleet (managed is false in fleet/bots.json)"));
    }
    let r = get(store, id)?;
    if !Path::new(&r.path).join("marshal").is_file() {
        return Err(format!("release {id} has no binary at {}", r.path));
    }
    let need = if b.role == "flagship" { "proven" } else { "gated" };
    if rank(&r.status) < rank(need) && force.is_none() {
        return Err(format!(
            "release {id} is '{}' and a {} needs '{need}' or later. Run the pipeline, or pass --force \"<reason>\" (it is logged).",
            r.status, b.role
        ));
    }
    let current = store.bot_state(bot).map_err(|e| e.to_string())?;
    let same = current.as_ref().map(|c| c.release.as_deref() == Some(id)).unwrap_or(false);
    // A new release on a bot is a new version on the ladder, so its rating is tracked apart.
    let version = match (version, same, current.as_ref().and_then(|c| c.version.clone())) {
        (Some(v), _, _) => v.to_string(),
        (None, true, Some(v)) => v,
        _ => store.next_version(bot).map_err(|e| e.to_string())?,
    };
    store.set_deployment(bot, id, &version, force.unwrap_or("deploy")).map_err(|e| e.to_string())?;
    store.event(Some(bot), "deploy", &json!({ "release": id, "version": version, "forced": force }));
    if let Some(pid) = restart_bot(root, bot) {
        eprintln!("{bot}: told pid {pid} to finish its match; the supervisor starts {id} as {version} next");
    } else {
        eprintln!("{bot}: not running now; it starts {id} as {version} when its supervisor runs");
    }
    Ok(version)
}

/// Puts a gated release on the ladder with every managed scout and canary bot.
pub fn canary(root: &Path, store: &Store, cfg: &Config, id: &str, force: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let scouts: Vec<&str> = cfg.bots.iter().filter(|b| b.managed && (b.role == "scout" || b.role == "canary")).map(|b| b.name.as_str()).collect();
    if scouts.is_empty() {
        return Err("no managed scout or canary bot in fleet/bots.json".into());
    }
    let mut out = Vec::new();
    for bot in scouts {
        out.push((bot.to_string(), deploy(root, store, cfg, bot, id, None, force)?));
    }
    let r = get(store, id)?;
    if rank(&r.status) < rank("canary") {
        store.set_release_status(id, "canary").map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// What to compare: a release id, or `bot:version` for games played outside the pipeline.
pub fn spec_filter(spec: &str) -> Filter {
    match spec.split_once(':') {
        Some((bot, version)) => Filter { bot: Some(bot.to_string()), version: Some(version.to_string()), no_ours: true, ..Filter::default() },
        None => Filter { release: Some(spec.to_string()), no_ours: true, ..Filter::default() },
    }
}

pub struct EvidenceOptions {
    pub baseline: Option<String>,
    pub min_games: i64,
    pub min_shared_opponents: usize,
    /// how sure we must be that the candidate is the better one
    pub confidence: f64,
}

impl Default for EvidenceOptions {
    fn default() -> Self {
        EvidenceOptions { baseline: None, min_games: 60, min_shared_opponents: 5, confidence: 0.90 }
    }
}

/// Ladder evidence: the candidate's ladder games against the baseline's, opponent by opponent.
pub fn evidence(store: &Store, id: &str, o: &EvidenceOptions) -> Result<Value, String> {
    let cand = get(store, id)?;
    let baseline = match &o.baseline {
        Some(b) => b.clone(),
        None => store.live_release().map_err(|e| e.to_string())?.map(|r| r.id).ok_or("no release is live: name the baseline with --baseline <release | bot:version>")?,
    };
    let c = store.records("opponent", &spec_filter(id)).map_err(|e| e.to_string())?;
    let b = store.records("opponent", &spec_filter(&baseline)).map_err(|e| e.to_string())?;
    let p = paired(&c, &b);
    let mut missing = Vec::new();
    if p.candidate.games() < o.min_games {
        missing.push(format!("{} of {} ladder games", p.candidate.games(), o.min_games));
    }
    if p.shared_opponents < o.min_shared_opponents {
        missing.push(format!("{} of {} opponents in common with the baseline", p.shared_opponents, o.min_shared_opponents));
    }
    let conf = p.confidence();
    let verdict = if !missing.is_empty() {
        "waiting"
    } else if p.diff > 0.0 && conf >= o.confidence {
        "pass"
    } else if conf <= 1.0 - o.confidence {
        "fail"
    } else {
        "undecided"
    };
    let report = json!({
        "verdict": verdict, "baseline": baseline, "waiting_for": missing,
        "candidate": { "games": p.candidate.games(), "wins": p.candidate.wins, "losses": p.candidate.losses, "draws": p.candidate.draws, "score": p.candidate.score() },
        "baseline_record": { "games": p.baseline.games(), "wins": p.baseline.wins, "losses": p.baseline.losses, "draws": p.baseline.draws, "score": p.baseline.score() },
        "shared_opponents": p.shared_opponents, "shared_candidate_games": p.shared_candidate_games, "shared_baseline_games": p.shared_baseline_games,
        "score_difference": p.diff, "standard_error": p.se, "confidence_better": conf, "needed": o.confidence, "ts_ms": now_ms(),
    });
    store.set_release_json(id, "evidence", &report).map_err(|e| e.to_string())?;
    if verdict == "pass" && rank(&cand.status) < rank("proven") && rank(&cand.status) >= rank("gated") {
        store.set_release_status(id, "proven").map_err(|e| e.to_string())?;
    }
    store.event(None, "release_evidence", &json!({ "release": id, "report": report }));
    Ok(report)
}

/// Makes a proven release the live one and deploys it to every managed flagship.
pub fn promote(root: &Path, store: &Store, cfg: &Config, id: &str, force: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let r = get(store, id)?;
    if r.status != "proven" && r.status != "live" && force.is_none() {
        return Err(format!(
            "release {id} is '{}', not 'proven': it needs to pass `fleet release gate` and `fleet release evidence` first, or --force \"<reason>\" (it is logged).",
            r.status
        ));
    }
    let previous = store.live_release().map_err(|e| e.to_string())?;
    if let Some(p) = &previous {
        if p.id != id {
            store.set_release_status(&p.id, "retired").map_err(|e| e.to_string())?;
        }
    }
    store.set_release_status(id, "live").map_err(|e| e.to_string())?;
    store.event(None, "release_promoted", &json!({ "release": id, "previous": previous.as_ref().map(|p| p.id.clone()), "forced": force }));
    let mut out = Vec::new();
    for b in cfg.bots.iter().filter(|b| b.role == "flagship") {
        if b.managed {
            out.push((b.name.clone(), deploy(root, store, cfg, &b.name, id, None, force.or(Some("promote")))?));
        } else {
            eprintln!("{} is a flagship the fleet does not manage: its owner deploys {} by hand", b.name, PathBuf::from(&r.path).join("marshal").display());
        }
    }
    Ok(out)
}
